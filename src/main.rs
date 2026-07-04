use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, TimeDelta, TimeZone, Utc};
use clap::{Parser, ValueEnum};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use regex::{Captures, Regex};
use rusqlite::Connection;

/// Location of Bear's application data, relative to the user's home directory.
const BEAR_APP_DATA: &str =
    "Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application Data";

/// Characters that must be percent-encoded inside a Markdown link destination.
const LINK_ESCAPES: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'(')
    .add(b')')
    .add(b'#')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'<')
    .add(b'>');

/// Export every note in Bear.app's database to a directory of Markdown files.
///
/// Notes are grouped into directories by their first tag, embedded images and
/// other attachments are copied next to the notes, and the Markdown is
/// rewritten to use standard links so the export can be opened directly in
/// apps like Obsidian or Logseq.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Directory the notes are exported to.
    #[arg(short, long, default_value = "bear-export")]
    output: PathBuf,

    /// Path to Bear's `database.sqlite` (defaults to Bear's standard location).
    #[arg(short, long)]
    database: Option<PathBuf>,

    /// Include notes that are in the trash.
    #[arg(long)]
    include_trashed: bool,

    /// Include archived notes.
    #[arg(long)]
    include_archived: bool,

    /// Put all notes directly into the output directory instead of one
    /// directory per tag.
    #[arg(long)]
    flat: bool,

    /// How to treat a note or attachment whose file name already exists in the
    /// output directory.
    #[arg(long, value_enum, default_value = "copy")]
    mode: Mode,
}

/// What to do when an export would land on a file name that is already taken in
/// the output directory. The names follow the idioms of rsync-like tools.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Never overwrite: write the note or attachment to a copy with a numerical
    /// suffix (` (2)`, ` (3)`, …) instead. This is the default and never
    /// touches files already in the output directory.
    Copy,
    /// Overwrite existing files in place, but leave any other files in the
    /// output directory alone.
    Update,
    /// Overwrite existing files in place and delete notes and attachments in
    /// the output directory that no longer correspond to anything in Bear, so
    /// the output becomes an exact mirror of the export.
    Mirror,
}

impl Mode {
    /// Whether an existing file should be overwritten rather than copied aside.
    fn overwrites(self) -> bool {
        matches!(self, Mode::Update | Mode::Mirror)
    }
}

struct Note {
    title: String,
    text: String,
    tags: Vec<String>,
    created: DateTime<Utc>,
    modified: DateTime<Utc>,
    /// Attachments Bear records for this note in its database, regardless of
    /// whether the note text references them.
    attachments: Vec<Attachment>,
}

/// A file embedded in a note, as recorded in Bear's `ZSFNOTEFILE` table. The
/// file lives on disk at `<attachment dir>/<unique_id>/<filename>`.
struct Attachment {
    unique_id: String,
    filename: String,
}

impl Attachment {
    /// Path of the file relative to an attachment directory.
    fn relative_path(&self) -> PathBuf {
        Path::new(&self.unique_id).join(&self.filename)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let database = match &cli.database {
        Some(path) => path.clone(),
        None => default_database_path()?,
    };
    if !database.is_file() {
        bail!(
            "no Bear database at {} — is Bear installed? A database can also be \
             given explicitly with --database",
            database.display()
        );
    }
    let app_data = database.parent().unwrap_or(Path::new("")).to_path_buf();

    // Bear may be running and writing to the database, so operate on a private
    // copy (including the WAL) instead of taking locks on the live file.
    let staging = tempfile::tempdir().context("failed to create a temporary directory")?;
    let connection = open_database_copy(&database, staging.path())?;

    let notes = load_notes(&connection, &cli)?;

    let exporter = Exporter {
        output: cli.output.clone(),
        attachment_dirs: vec![
            app_data.join("Local Files").join("Note Images"),
            app_data.join("Local Files").join("Note Files"),
        ],
        flat: cli.flat,
        mode: cli.mode,
    };
    let mut attachments = 0;
    let mut kept = HashSet::new();
    for note in &notes {
        let written = exporter
            .export(note)
            .with_context(|| format!("failed to export note “{}”", note.title))?;
        attachments += written.attachments;
        if cli.mode == Mode::Mirror {
            for path in written.files {
                kept.insert(path.canonicalize().unwrap_or(path));
            }
        }
    }

    let deleted = if cli.mode == Mode::Mirror {
        prune(&cli.output, &kept).context("failed to prune stale files from the output")?
    } else {
        0
    };

    print!(
        "Exported {} note(s) and {attachments} attachment(s) to {}",
        notes.len(),
        cli.output.display()
    );
    if deleted > 0 {
        print!("; deleted {deleted} stale file(s)");
    }
    println!(".");
    Ok(())
}

/// Deletes any file under `output` that is not in `kept`, then removes the
/// directories left empty by those deletions. Returns the number of files
/// deleted. Used by `Mode::Mirror` to make the output an exact replica of the
/// export.
fn prune(output: &Path, kept: &HashSet<PathBuf>) -> Result<usize> {
    if !output.is_dir() {
        return Ok(0);
    }
    let mut deleted = 0;
    prune_dir(output, kept, &mut deleted)?;
    Ok(deleted)
}

/// Recursively prunes `dir`, returning whether it is empty afterwards so the
/// caller can remove directories that no longer hold any kept files.
fn prune_dir(dir: &Path, kept: &HashSet<PathBuf>, deleted: &mut usize) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // Never prune version-control metadata: the output directory is often a
        // Git repository, and deleting `.git` would destroy its history.
        if entry.file_name() == ".git" {
            empty = false;
            continue;
        }
        if entry.file_type()?.is_dir() {
            if prune_dir(&path, kept, deleted)? {
                fs::remove_dir(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            } else {
                empty = false;
            }
        } else if kept.contains(&path.canonicalize().unwrap_or_else(|_| path.clone())) {
            empty = false;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            *deleted += 1;
        }
    }
    Ok(empty)
}

/// The standard location of Bear's database on macOS.
fn default_database_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(BEAR_APP_DATA).join("database.sqlite"))
}

/// Copies the database (and its `-wal`/`-shm` sidecars, if any) into
/// `staging` and opens the copy.
fn open_database_copy(database: &Path, staging: &Path) -> Result<Connection> {
    let copy = staging.join("database.sqlite");
    fs::copy(database, &copy).with_context(|| format!("failed to copy {}", database.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut name = database.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        let sidecar = database.with_file_name(name);
        if sidecar.is_file() {
            let mut copy_name = OsString::from("database.sqlite");
            copy_name.push(suffix);
            fs::copy(&sidecar, staging.join(copy_name))
                .with_context(|| format!("failed to copy {}", sidecar.display()))?;
        }
    }
    Connection::open(&copy).context("failed to open the Bear database")
}

fn load_notes(connection: &Connection, cli: &Cli) -> Result<Vec<Note>> {
    // `ZENCRYPTED` does not exist in older Bear schemas.
    let encrypted = if has_column(connection, "ZSFNOTE", "ZENCRYPTED") {
        "ZENCRYPTED"
    } else {
        "0"
    };
    let mut conditions = vec!["ZTEXT IS NOT NULL"];
    if !cli.include_trashed {
        conditions.push("ZTRASHED = 0");
    }
    if !cli.include_archived {
        conditions.push("ZARCHIVED = 0");
    }
    let query = format!(
        "SELECT Z_PK, ZTITLE, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE, {encrypted} AS encrypted \
         FROM ZSFNOTE WHERE {} ORDER BY ZCREATIONDATE",
        conditions.join(" AND ")
    );

    let mut attachments = load_attachments(connection)?;

    let mut statement = connection
        .prepare(&query)
        .context("failed to query the notes table — is this a Bear database?")?;
    let mut rows = statement.query([])?;
    let mut notes = Vec::new();
    let mut encrypted_count = 0usize;
    while let Some(row) = rows.next()? {
        if row.get::<_, Option<i64>>("encrypted")?.unwrap_or(0) != 0 {
            encrypted_count += 1;
            continue;
        }
        let pk: i64 = row.get("Z_PK")?;
        let text: String = row.get("ZTEXT")?;
        let title = row
            .get::<_, Option<String>>("ZTITLE")?
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| derive_title(&text));
        notes.push(Note {
            tags: extract_tags(&text),
            created: from_core_data_timestamp(
                row.get::<_, Option<f64>>("ZCREATIONDATE")?
                    .unwrap_or_default(),
            ),
            modified: from_core_data_timestamp(
                row.get::<_, Option<f64>>("ZMODIFICATIONDATE")?
                    .unwrap_or_default(),
            ),
            attachments: attachments.remove(&pk).unwrap_or_default(),
            title,
            text,
        });
    }
    if encrypted_count > 0 {
        eprintln!(
            "warning: skipped {encrypted_count} encrypted note(s); \
             Bear does not store their text in readable form"
        );
    }
    Ok(notes)
}

/// Loads every note's attachments from Bear's `ZSFNOTEFILE` table, keyed by the
/// note's primary key. Returns an empty map on schemas that predate the table.
fn load_attachments(connection: &Connection) -> Result<HashMap<i64, Vec<Attachment>>> {
    // Older Bear schemas may lack the table (or these columns); degrade to the
    // text-based rewriting alone rather than failing the whole export.
    if !has_column(connection, "ZSFNOTEFILE", "ZUNIQUEIDENTIFIER")
        || !has_column(connection, "ZSFNOTEFILE", "ZFILENAME")
        || !has_column(connection, "ZSFNOTEFILE", "ZNOTE")
    {
        return Ok(HashMap::new());
    }

    let mut statement = connection.prepare(
        "SELECT ZNOTE, ZUNIQUEIDENTIFIER, ZFILENAME FROM ZSFNOTEFILE \
         WHERE ZNOTE IS NOT NULL AND ZUNIQUEIDENTIFIER IS NOT NULL AND ZFILENAME IS NOT NULL",
    )?;
    let mut rows = statement.query([])?;
    let mut attachments: HashMap<i64, Vec<Attachment>> = HashMap::new();
    while let Some(row) = rows.next()? {
        let note: i64 = row.get("ZNOTE")?;
        let unique_id: String = row.get("ZUNIQUEIDENTIFIER")?;
        let filename: String = row.get("ZFILENAME")?;
        if filename.trim().is_empty() {
            continue;
        }
        attachments.entry(note).or_default().push(Attachment {
            unique_id,
            filename,
        });
    }
    Ok(attachments)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> bool {
    connection
        .prepare(&format!("SELECT {column} FROM {table} LIMIT 0"))
        .is_ok()
}

/// Core Data stores timestamps as seconds since 2001-01-01 00:00:00 UTC.
fn from_core_data_timestamp(seconds: f64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).unwrap()
        + TimeDelta::milliseconds((seconds * 1000.0).round() as i64)
}

fn derive_title(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled")
        .to_string()
}

/// Bear tags (`#tag`, possibly nested like `#work/projects`) as they appear in
/// the note text, in order of first appearance.
fn extract_tags(text: &str) -> Vec<String> {
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[\s(])#([\p{L}\p{N}_][\p{L}\p{N}\p{M}_/-]*)").unwrap());
    let mut tags = Vec::new();
    for captures in TAG.captures_iter(text) {
        let tag = captures[1].trim_end_matches('/').to_string();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

struct Exporter {
    output: PathBuf,
    attachment_dirs: Vec<PathBuf>,
    flat: bool,
    mode: Mode,
}

/// The result of exporting a single note.
struct Export {
    /// Number of attachments copied for the note.
    attachments: usize,
    /// Files written for the note (the Markdown file and its attachments), used
    /// by `Mode::Mirror` to know what to keep.
    files: Vec<PathBuf>,
}

impl Exporter {
    /// Writes the note (and the attachments it references) to disk.
    fn export(&self, note: &Note) -> Result<Export> {
        let directory = self.note_directory(note);
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let path = resolve_path(
            &directory,
            &format!("{}.md", sanitize_file_name(&note.title)),
            self.mode.overwrites(),
        );
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        let mut rewriter = AttachmentRewriter {
            source_dirs: &self.attachment_dirs,
            assets_dir: directory.join("assets").join(&stem),
            assets_prefix: Path::new("assets").join(&stem),
            copied: HashMap::new(),
            written: Vec::new(),
            overwrite: self.mode.overwrites(),
            by_filename: note
                .attachments
                .iter()
                .map(|attachment| (attachment.filename.clone(), attachment.relative_path()))
                .collect(),
        };
        let mut body = rewriter.rewrite(&note.text);
        append_orphan_attachments(&mut rewriter, note, &mut body);

        let mut contents = front_matter(note);
        contents.push_str(&body);
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;

        let attachments = rewriter.copied.len();
        let mut files = rewriter.written;
        files.push(path);
        Ok(Export { attachments, files })
    }

    /// Notes live in a directory tree derived from their first tag; untagged
    /// notes go directly into the output directory.
    fn note_directory(&self, note: &Note) -> PathBuf {
        if self.flat {
            return self.output.clone();
        }
        match note.tags.first() {
            Some(tag) => tag
                .split('/')
                .filter(|segment| !segment.is_empty())
                .fold(self.output.clone(), |directory, segment| {
                    directory.join(sanitize_file_name(segment))
                }),
            None => self.output.clone(),
        }
    }
}

/// Rewrites attachment references in a note's text to standard Markdown links
/// pointing into the note's `assets` directory, copying the referenced files
/// there as a side effect.
struct AttachmentRewriter<'a> {
    source_dirs: &'a [PathBuf],
    /// Where attachment copies are written.
    assets_dir: PathBuf,
    /// The same directory as seen from the note file, used in links.
    assets_prefix: PathBuf,
    /// Source path → rewritten link, so a file referenced twice is copied once.
    copied: HashMap<PathBuf, String>,
    /// Destination paths of the attachments copied, in the order they were
    /// written, so `Mode::Mirror` knows which files to keep.
    written: Vec<PathBuf>,
    /// Overwrite existing attachments instead of writing suffixed copies.
    overwrite: bool,
    /// File name → path relative to the attachment store (`<unique id>/<name>`),
    /// from Bear's database, used to resolve references that don't already spell
    /// out the attachment store path.
    by_filename: HashMap<String, PathBuf>,
}

impl AttachmentRewriter<'_> {
    fn rewrite(&mut self, text: &str) -> String {
        // Bear 1.x embeds attachments with a proprietary `[image:…]`/`[file:…]`
        // syntax; turn those into standard Markdown.
        static BEAR_TOKEN: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\[(image|file):([^\]\n]+)\]").unwrap());
        let text = BEAR_TOKEN.replace_all(text, |captures: &Captures| {
            match self.import(&captures[2], true) {
                Some((link, name)) if &captures[1] == "image" => format!("![{name}]({link})"),
                Some((link, name)) => format!("[{name}]({link})"),
                None => {
                    eprintln!("warning: attachment {} not found on disk", &captures[2]);
                    captures[0].to_string()
                }
            }
        });

        // Bear 2.x already uses Markdown links whose targets are paths inside
        // its attachment store; re-target those at the copied files. Links to
        // anything else (URLs, other notes) are left untouched.
        static MARKDOWN_LINK: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(!?)\[([^\]\n]*)\]\(([^)\n]+)\)").unwrap());
        MARKDOWN_LINK
            .replace_all(&text, |captures: &Captures| {
                match self.import(&captures[3], false) {
                    Some((link, name)) => {
                        let label = match &captures[2] {
                            "" if captures[1].is_empty() => name.as_str(),
                            label => label,
                        };
                        format!("{}[{label}]({link})", &captures[1])
                    }
                    None => captures[0].to_string(),
                }
            })
            .into_owned()
    }

    /// Copies the attachment at `target` (a path relative to Bear's attachment
    /// store, possibly percent-encoded) into the assets directory. Returns the
    /// link to use in Markdown and the attachment's file name, or `None` if
    /// `target` does not point at an attachment.
    fn import(&mut self, target: &str, allow_filename_fallback: bool) -> Option<(String, String)> {
        let decoded = percent_decode_str(target).decode_utf8().ok()?;
        let source = self.resolve_source(Path::new(decoded.as_ref()), allow_filename_fallback)?;
        let name = source.file_name()?.to_string_lossy().into_owned();
        if let Some(link) = self.copied.get(&source) {
            return Some((link.clone(), name));
        }

        if let Err(error) = fs::create_dir_all(&self.assets_dir) {
            eprintln!(
                "warning: failed to create {}: {error}",
                self.assets_dir.display()
            );
            return None;
        }
        let destination = resolve_path(&self.assets_dir, &name, self.overwrite);
        if let Err(error) = fs::copy(&source, &destination) {
            eprintln!("warning: failed to copy {}: {error}", source.display());
            return None;
        }
        let link = encode_link(&self.assets_prefix.join(destination.file_name()?));
        self.copied.insert(source, link.clone());
        self.written.push(destination);
        Some((link, name))
    }

    /// Locates the on-disk file a reference points at. A `relative` path made of
    /// plain components is looked up directly in the attachment directories.
    /// When `allow_filename_fallback` is set and that fails (or the path is
    /// unsafe to join), the reference's file name is matched against the
    /// attachments Bear records for the note — used only for Bear's unambiguous
    /// `[image:…]`/`[file:…]` tokens, never for arbitrary Markdown links that
    /// might point at unrelated external targets.
    fn resolve_source(&self, relative: &Path, allow_filename_fallback: bool) -> Option<PathBuf> {
        if relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            && let Some(source) = self.source_for(relative)
        {
            return Some(source);
        }
        if !allow_filename_fallback {
            return None;
        }
        let name = relative.file_name()?.to_string_lossy();
        let known = self.by_filename.get(name.as_ref())?;
        self.source_for(known)
    }

    /// The first attachment directory that actually contains `relative`.
    fn source_for(&self, relative: &Path) -> Option<PathBuf> {
        self.source_dirs
            .iter()
            .map(|directory| directory.join(relative))
            .find(|candidate| candidate.is_file())
    }
}

/// Copies any of the note's recorded attachments that the note text did not
/// already reference, appending a Markdown link for each so nothing Bear
/// considers embedded is dropped from the export.
fn append_orphan_attachments(rewriter: &mut AttachmentRewriter<'_>, note: &Note, body: &mut String) {
    let mut appended = Vec::new();
    for attachment in &note.attachments {
        let relative = attachment.relative_path();
        match rewriter.source_for(&relative) {
            // Already copied while rewriting the text — it is referenced inline.
            Some(source) if rewriter.copied.contains_key(&source) => {}
            Some(_) => {
                if let Some(link) = rewriter.import(&relative.to_string_lossy(), false) {
                    appended.push(link);
                }
            }
            None => eprintln!(
                "warning: attachment {} of note “{}” not found on disk",
                attachment.filename, note.title
            ),
        }
    }
    if appended.is_empty() {
        return;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    for (link, name) in appended {
        if is_image(&name) {
            body.push_str(&format!("![{name}]({link})\n"));
        } else {
            body.push_str(&format!("[{name}]({link})\n"));
        }
    }
}

/// Whether a file name looks like an image, based on its extension.
fn is_image(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "heic" | "heif" | "webp" | "tiff" | "tif"
                    | "bmp" | "svg"
            )
        })
        .unwrap_or(false)
}

/// YAML front matter understood by Obsidian, Logseq, and similar apps.
fn front_matter(note: &Note) -> String {
    let mut front_matter = String::from("---\n");
    front_matter.push_str(&format!(
        "title: \"{}\"\n",
        note.title.replace('\\', "\\\\").replace('"', "\\\"")
    ));
    front_matter.push_str(&format!(
        "created: {}\n",
        note.created.to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    front_matter.push_str(&format!(
        "modified: {}\n",
        note.modified.to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    if !note.tags.is_empty() {
        front_matter.push_str("tags:\n");
        for tag in &note.tags {
            front_matter.push_str(&format!("  - {tag}\n"));
        }
    }
    front_matter.push_str("---\n\n");
    front_matter
}

/// A version of `name` that is safe to use as a file or directory name.
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = collapsed
        .trim_matches(['.', ' '])
        .chars()
        .take(120)
        .collect();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed
    }
}

/// `directory/file_name`, with ` (2)`, ` (3)`, … appended to the stem if the
/// name is already taken — unless `overwrite` is set, in which case the plain
/// `directory/file_name` is returned even when it already exists.
fn resolve_path(directory: &Path, file_name: &str, overwrite: bool) -> PathBuf {
    if overwrite {
        return directory.join(file_name);
    }
    let stem = Path::new(file_name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let extension = Path::new(file_name)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let mut candidate = directory.join(file_name);
    let mut counter = 2;
    while candidate.exists() {
        candidate = directory.join(format!("{stem} ({counter}){extension}"));
        counter += 1;
    }
    candidate
}

/// Percent-encodes a relative path for use as a Markdown link destination.
fn encode_link(path: &Path) -> String {
    path.iter()
        .map(|component| {
            utf8_percent_encode(&component.to_string_lossy(), LINK_ESCAPES).to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_stale_files_but_keeps_git() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path();

        // A note that should survive because it is in `kept`.
        let kept_note = output.join("keep.md");
        fs::write(&kept_note, "keep").unwrap();
        // A stale note that no longer corresponds to anything in Bear.
        let stale_note = output.join("stale.md");
        fs::write(&stale_note, "stale").unwrap();
        // A Git repository backing the output directory.
        let git = output.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        let head = git.join("HEAD");
        fs::write(&head, "ref: refs/heads/main").unwrap();

        let mut kept = HashSet::new();
        kept.insert(kept_note.canonicalize().unwrap());

        let deleted = prune(output, &kept).unwrap();

        assert_eq!(deleted, 1);
        assert!(kept_note.exists());
        assert!(!stale_note.exists());
        // The Git metadata must be left completely untouched.
        assert!(git.is_dir());
        assert!(head.exists());
        assert!(git.join("objects").is_dir());
    }
}
