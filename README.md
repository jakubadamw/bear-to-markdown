# bear-to-markdown

A command-line tool that exports every note stored in
[Bear.app](https://bear.app)'s database into a directory structure of plain
Markdown files, together with any files (images, PDFs, …) embedded in them.

The output is meant to be dropped straight into note apps that use Markdown
as their primary storage format (Obsidian, Logseq, …):

- notes are grouped into directories by their first tag (nested tags like
  `#work/projects` become nested directories);
- each note gets YAML front matter with its title, creation/modification
  timestamps, and tags;
- Bear's proprietary `[image:…]`/`[file:…]` embeds, as well as Bear 2's
  links into its internal attachment store, are rewritten to standard
  relative Markdown links, and the referenced files are copied into an
  `assets/<note name>/` directory next to the note.

## Usage

```console
$ bear-to-markdown [--output <DIR>]
```

By default the tool reads Bear's live database from its standard location
(`~/Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application
Data/database.sqlite`) — it works on a temporary copy, so it is safe to run
while Bear is open — and writes the export to `./bear-export`.

Options:

| Flag | Meaning |
| ---- | ------- |
| `-o, --output <DIR>` | Directory the notes are exported to (default: `bear-export`). |
| `-d, --database <FILE>` | Explicit path to a `database.sqlite` to read instead of the default. |
| `--include-trashed` | Also export notes that are in the trash. |
| `--include-archived` | Also export archived notes. |
| `--flat` | Put all notes directly into the output directory instead of one directory per tag. |
| `--mode <MODE>` | How to treat a note or attachment whose file name already exists in the output directory (default: `copy`). |

`--mode` takes one of three values, named after the idioms of rsync-like
tools:

| Mode | Meaning |
| ---- | ------- |
| `copy` (default) | Never overwrite; write the note or attachment to a copy with a numerical suffix (` (2)`, ` (3)`, …) instead. Files already in the output directory are never touched. |
| `update` | Overwrite existing files in place, but leave any other files in the output directory alone. |
| `mirror` | Overwrite existing files in place **and** delete notes and attachments in the output directory that no longer correspond to anything in Bear, so the output becomes an exact replica of the export. |

Encrypted notes are skipped (Bear does not store their text in readable
form); a warning is printed for each attachment that is referenced but
missing on disk.

## Building

```console
$ cargo build --release
```

The binary ends up in `target/release/bear-to-markdown`. SQLite is bundled,
so there are no runtime dependencies.
