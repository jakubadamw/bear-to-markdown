use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, TimeDelta, TimeZone, Utc};
use clap::Parser;
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
}

struct Note {
    title: String,
    text: String,
    tags: Vec<String>,
    created: DateTime<Utc>,
    modified: DateTime<Utc>,
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
    };
    let mut attachments = 0;
    for note in &notes {
        attachments += exporter
            .export(note)
            .with_context(|| format!("failed to export note “{}”", note.title))?;
    }

    println!(
        "Exported {} note(s) and {attachments} attachment(s) to {}.",
        notes.len(),
        cli.output.display()
    );
    Ok(())
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
        "SELECT ZTITLE, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE, {encrypted} AS encrypted \
         FROM ZSFNOTE WHERE {} ORDER BY ZCREATIONDATE",
        conditions.join(" AND ")
    );

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
}

impl Exporter {
    /// Writes the note (and the attachments it references) to disk and returns
    /// the number of attachments copied.
    fn export(&self, note: &Note) -> Result<usize> {
        let directory = self.note_directory(note);
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let path = unique_path(
            &directory,
            &format!("{}.md", sanitize_file_name(&note.title)),
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
        };
        let body = rewriter.rewrite(&note.text);

        let mut contents = front_matter(note);
        contents.push_str(&body);
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(rewriter.copied.len())
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
}

impl AttachmentRewriter<'_> {
    fn rewrite(&mut self, text: &str) -> String {
        // Bear 1.x embeds attachments with a proprietary `[image:…]`/`[file:…]`
        // syntax; turn those into standard Markdown.
        static BEAR_TOKEN: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\[(image|file):([^\]\n]+)\]").unwrap());
        let text = BEAR_TOKEN.replace_all(text, |captures: &Captures| {
            match self.import(&captures[2]) {
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
                match self.import(&captures[3]) {
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
    fn import(&mut self, target: &str) -> Option<(String, String)> {
        let decoded = percent_decode_str(target).decode_utf8().ok()?;
        let relative = Path::new(decoded.as_ref());
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        let source = self
            .source_dirs
            .iter()
            .map(|directory| directory.join(relative))
            .find(|candidate| candidate.is_file())?;
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
        let destination = unique_path(&self.assets_dir, &name);
        if let Err(error) = fs::copy(&source, &destination) {
            eprintln!("warning: failed to copy {}: {error}", source.display());
            return None;
        }
        let link = encode_link(&self.assets_prefix.join(destination.file_name()?));
        self.copied.insert(source, link.clone());
        Some((link, name))
    }
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
/// name is already taken.
fn unique_path(directory: &Path, file_name: &str) -> PathBuf {
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
