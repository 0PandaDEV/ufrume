<img width="3000" height="1000" alt="Twitter Header" src="https://github.com/user-attachments/assets/04e5058c-affe-4192-ba5b-6326574033f6" />

<br>
<br>

A multithreaded CLI tool to organize your music files into a folder structure defined by you.

<table>
<tr>
<td valign="top">

**BEFORE**

```
music/
├── DEEP.flac
├── The Storm.flac
├── Walk Slowly.flac
├── 2U.flac
├── Good Morning Vietnam.flac
├── Too Much (extended mix).flac
├── jungle_compilation_track01.flac
└── [500+ more randomly named files...]
```

</td>
<td valign="top">

**AFTER**

```
music/
├── Bad Computer/
│   └── 2020 - 2U/
│       └── 01 - 2U.flac
├── Example/
│   └── 2021 - DEEP/
│       └── 01 - DEEP.flac
├── TheFatRat/
│   └── 2019 - The Storm/
│       └── 01 - The Storm.flac
├── Ballpoint/
│   └── 2022 - Walk Slowly/
│       └── 01 - Walk Slowly.flac
├── Marc Benjamin/
│   └── 2023 - Too Much/
│       └── 01 - Too Much (extended mix).flac
├── Shotgun Willy/
│   └── 2020 - Good Morning Vietnam/
│       └── 01 - Good Morning Vietnam.flac
└── Compilations/
    └── Welcome to the Jungle- The Ultimate Jungle Cakes Drum & Bass Compilation/
        ├── 01 - DJ Deekline & Ed Solo feat. Top Cat - Bad Boys.flac
        ├── 02 - DJ Deekline & Ed Solo - No No No (Serial Killaz remix).flac
        └── 12 - Ricky Tuffy feat. Ras Mc Bean - Brighter Day.flac
```

</td>
</tr>
</table>

# Installation

### Install script (Linux & macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/0PandaDEV/ufrume/main/scripts/install.sh | sh
```

Installs to `~/.local/bin` by default.

Update later with `ufrume update`.

### From [crates.io](https://crates.io/crates/ufrume)

```bash
cargo install ufrume
```

### From [AUR](https://aur.archlinux.org/packages/ufrume)

```bash
yay -S ufrume
```

### From [Homebrew](https://github.com/0PandaDEV/homebrew-repo)

```bash
brew tap 0PandaDEV/repo
brew install ufrume
```

### From [Github Releases](https://github.com/0PandaDEV/ufrume/releases/latest)

Download the binary for your OS and architecture, then follow the installation steps below:

<details>
<summary><strong>macOS</strong></summary>

```bash
# Extract and install to /usr/local/bin (recommended)
tar -xzf ufrume-macos-*.tar.gz
sudo mv ufrume /usr/local/bin/

# Or install to user directory (no sudo required)
tar -xzf ufrume-macos-*.tar.gz
mkdir -p ~/.local/bin
mv ufrume ~/.local/bin/
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

</details>

<details>
<summary><strong>Windows</strong></summary>

#### Extract the zip file then run these commands in the PowerShell

```pwsh
mkdir "$env:USERPROFILE\bin"
move ufrume.exe "$env:USERPROFILE\bin\"

# Add to PATH (restart terminal after this)
$env:PATH += ";$env:USERPROFILE\bin"
[Environment]::SetEnvironmentVariable("PATH", $env:PATH, [EnvironmentVariableTarget]::User)
```

</details>

<details>
<summary><strong>Linux</strong></summary>

```bash
# Extract and install to /usr/local/bin (recommended)
tar -xzf ufrume-linux-*.tar.gz
sudo mv ufrume /usr/local/bin/

# Or install to user directory (no sudo required)
tar -xzf ufrume-linux-*.tar.gz
mkdir -p ~/.local/bin
mv ufrume ~/.local/bin/
# ~/.local/bin is usually already in PATH on most distributions
```

</details>

# Configuration

Ufrume uses a TOML configuration file to customize how your music files are organized. The configuration file is automatically created at:

- **Linux/macOS**: `~/.config/ufrume/config.toml`
- **Windows**: `%APPDATA%\ufrume\config.toml`

### Default Configuration

When you first run ufrume, it creates a default configuration file:

```toml
[organization]
structure = "{artist}/{year} - {album}/{track:02} - {title}"
compilation_structure = "Compilations/{album}/{track:02} - {artist} - {title}"
fallback_structure = "{filename}"

[rules]
handle_missing_metadata = "fallback"
handle_duplicates = "skip"

[formatting]
max_filename_length = 255

[formatting.replace_chars]
"/" = "-"
":" = "-"
"?" = ""
```

### Configuration Options

#### Organization

| Option                  | Description                                       | Example                                                  |
|-------------------------|---------------------------------------------------|----------------------------------------------------------|
| `structure`             | Main folder structure template for regular albums | `"{artist}/{year} - {album}/{track:02} - {title}"`       |
| `compilation_structure` | Structure for compilation albums (optional)       | `"Compilations/{album}/{track:02} - {artist} - {title}"` |
| `fallback_structure`    | Structure used when metadata is missing           | `"{filename}"`                                           |

#### Rules

| Option                    | Description                         | Values                              |
|---------------------------|-------------------------------------|-------------------------------------|
| `handle_missing_metadata` | What to do when metadata is missing | `"fallback"`, `"skip"`              |
| `handle_duplicates`       | How to handle duplicate files       | `"skip"`, `"overwrite"`, `"rename"` |

#### Formatting

| Option                | Description                                            | Default         |
|-----------------------|--------------------------------------------------------|-----------------|
| `max_filename_length` | Maximum length for filenames (characters)              | `255`           |
| `replace_chars`       | Character replacements for invalid filename characters | See table below |

#### Character Replacements

The `replace_chars` section defines how invalid filesystem characters are handled:

| Character | Replacement | Reason                     |
|-----------|-------------|----------------------------|
| `/`       | `-`         | Path separator conflict    |
| `:`       | `-`         | Invalid on Windows         |
| `?`       | (removed)   | Invalid filename character |

### Available Template Variables

You can use these variables in your structure templates:

- `{artist}` - Track artist
- `{album}` - Album name
- `{title}` - Track title
- `{track}` - Track number
- `{track:02}` - Track number with zero padding (01, 02, etc.)
- `{year}` - Release year
- `{genre}` - Music genre
- `{filename}` - Original filename (without extension)

### Example Configurations

<details>
<summary><strong>Artist/Album Structure</strong></summary>

```toml
[organization]
structure = "{artist}/{album}/{track:02} - {title}"
fallback_structure = "Unknown/{filename}"
```

Result: `Beatles/Abbey Road/01 - Come Together.mp3`

</details>

<details>
<summary><strong>Genre-Based Structure</strong></summary>

```toml
[organization]
structure = "{genre}/{artist}/{year} - {album}/{track:02} - {title}"
fallback_structure = "Unknown/{filename}"
```

Result: `Rock/Beatles/1969 - Abbey Road/01 - Come Together.mp3`

</details>

<details>
<summary><strong>Year-First Structure</strong></summary>

```toml
[organization]
structure = "{year}/{artist} - {album}/{track:02} - {title}"
fallback_structure = "Unknown Year/{filename}"
```

Result: `1969/Beatles - Abbey Road/01 - Come Together.mp3`

</details>

### Custom Character Replacements

You can add more character replacements for specific needs:

```toml
[formatting.replace_chars]
"/" = "-"
":" = "-"
"?" = ""
"*" = ""
"<" = "("
">" = ")"
"|" = "-"
"\"" = "'"
```

### Tips

- Use `{track:02}` for zero-padded track numbers (01, 02, 03...)
- Set `compilation_structure` to `null` to use the main structure for compilations
- Test your configuration with a small subset of files first
- The `fallback_structure` is crucial for files with missing metadata
