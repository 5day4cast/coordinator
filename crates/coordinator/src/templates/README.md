# Templates & Frontend Assets

Server-rendered HTML using [Maud](https://maud.lambda.xyz/) with co-located JavaScript and CSS.

## Structure

```
templates/
├── shared/           # JS utilities used across templates
│   └── *.js
├── layouts/          # Page shells
│   └── base/
│       ├── mod.rs
│       └── base.js
├── components/       # Reusable UI
│   ├── navbar.rs     # Simple template (no JS/CSS)
│   └── modals/       # Folder when JS/CSS needed
│       ├── mod.rs
│       ├── modals.js
│       └── modals.css  # Component-specific styles
├── pages/
│   └── entries/
│       ├── mod.rs
│       ├── entries.js
│       └── entries.css
└── fragments/        # HTMX partials
    └── *.rs
```

## Conventions

**No JS/CSS needed:** single `foo.rs` file

**JS/CSS needed:** folder with `foo/mod.rs` + `foo.js` and/or `foo.css`

**Shared JS:** put in `shared/` directory

## Build Process

At compile time, `build.rs`:

**JavaScript:**
1. Finds all `.js` files in `templates/`
2. Concatenates them (shared first, base.js last)
3. Minifies and hashes for cache busting
4. Outputs to `crates/public_ui/`

**CSS:**
1. Starts with `crates/public_ui/styles.css` (base/global styles)
2. Appends any `.css` files found in `templates/`
3. Minifies into `styles.min.css`

## Loader Pattern

External dependencies (WASM, large libraries) go in `crates/public_ui/loader.js`:

```javascript
// loader.js - not bundled, loaded directly
import init, { NostrClientWrapper } from '/ui/dist/client_validator.js';

window.NostrClientWrapper = NostrClientWrapper;
window.initWasm = async () => { await init(); };

// Load app bundle after deps ready
const script = document.createElement('script');
script.src = '/ui/app.min.js';
document.head.appendChild(script);
```

Then in template JS, access via `window`:
```javascript
const client = new window.NostrClientWrapper();
```

**Why?**
- Keeps ES module imports out of bundled code
- Single place to manage WASM/external versions
- App code stays clean and testable

## Adding JavaScript

1. Convert `pages/foo.rs` to `pages/foo/mod.rs`
2. Create `pages/foo/foo.js` with your code
3. Rebuild - automatically bundled

## Adding CSS

**Global styles:** Edit `crates/public_ui/styles.css`

**Component styles:** Add `mycomponent.css` next to `mod.rs`:
```
components/
└── widget/
    ├── mod.rs
    ├── widget.js
    └── widget.css   # Styles specific to this component
```

Reference in layout:
```rust
link rel="stylesheet" href="/ui/styles.min.css";
```

## Key Files

| File | Purpose |
|------|---------|
| `build.rs` | Bundles JS/CSS at compile time |
| `crates/public_ui/loader.js` | WASM/external deps, loads app bundle |
| `crates/public_ui/styles.css` | Base/global styles (manual) |
| `crates/public_ui/app.min.js` | Generated JS bundle |
| `crates/public_ui/styles.min.css` | Generated CSS bundle |
| `shared/*.js` | Utilities for all templates |
| `layouts/base/base.js` | App init (runs last) |

---

## Porting to Another Project

### 1. Add build dependencies

```toml
[build-dependencies]
minify-js = "0.6"
walkdir = "2.5"
sha2 = "0.10"
hex = "0.4"
```

### 2. Create build.rs

```rust
use minify_js::{minify, Session, TopLevelMode};
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};
use walkdir::WalkDir;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let templates = Path::new(&manifest).join("src/templates");
    let output = Path::new(&manifest).join("static");

    if !templates.exists() { return; }

    for entry in WalkDir::new(&templates).into_iter().filter_map(|e| e.ok()) {
        let ext = entry.path().extension().and_then(|e| e.to_str());
        if matches!(ext, Some("js") | Some("css")) {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    let _ = fs::create_dir_all(&output);
    build_js(&templates, &output);
    build_css(&templates, &output);
}

fn build_js(templates: &Path, output: &Path) {
    let mut files: Vec<_> = WalkDir::new(templates)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |e| e == "js"))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();

    let mut combined = String::new();
    for file in files {
        if let Ok(content) = fs::read_to_string(&file) {
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    if combined.is_empty() { return; }

    let minified = try_minify_js(&combined).unwrap_or(combined);
    let hash = hex::encode(Sha256::digest(minified.as_bytes()));
    let _ = fs::write(output.join(format!("app.{}.min.js", &hash[..8])), &minified);
    let _ = fs::write(output.join("app.min.js"), &minified);
}

fn build_css(templates: &Path, output: &Path) {
    let mut combined = String::new();

    // Base styles first
    let base = output.join("styles.css");
    if base.exists() {
        if let Ok(content) = fs::read_to_string(&base) {
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    // Then template CSS
    let mut files: Vec<_> = WalkDir::new(templates)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |e| e == "css"))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();

    for file in files {
        if let Ok(content) = fs::read_to_string(&file) {
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    if combined.is_empty() { return; }

    let minified = minify_css(&combined);
    let hash = hex::encode(Sha256::digest(minified.as_bytes()));
    let _ = fs::write(output.join(format!("styles.{}.min.css", &hash[..8])), &minified);
    let _ = fs::write(output.join("styles.min.css"), &minified);
}

fn try_minify_js(src: &str) -> Option<String> {
    let session = Session::new();
    let mut out = Vec::new();
    minify(&session, TopLevelMode::Module, src.as_bytes(), &mut out).ok()?;
    String::from_utf8(out).ok()
}

fn minify_css(css: &str) -> String {
    // Simple minification: remove comments and excess whitespace
    let mut out = String::new();
    let mut in_comment = false;
    let mut chars = css.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') { chars.next(); in_comment = false; }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') { chars.next(); in_comment = true; continue; }
        if c.is_whitespace() {
            if !out.ends_with(|ch: char| ch.is_whitespace() || "{:;,".contains(ch)) {
                if chars.peek().map_or(false, |&n| !"{}:;,".contains(n)) { out.push(' '); }
            }
            continue;
        }
        out.push(c);
    }
    out
}
```

### 3. Create loader.js (if using WASM/external deps)

```javascript
// static/loader.js
import * as myLib from 'https://cdn.example.com/lib.js';
window.myLib = myLib;

const script = document.createElement('script');
script.type = 'module';
script.src = '/static/app.min.js';
document.head.appendChild(script);
```

### 4. Serve and reference

```rust
// Router
.nest_service("/static", ServeDir::new("static"))

// Template - load loader.js, NOT app.min.js directly
script type="module" src="/static/loader.js" {}
link rel="stylesheet" href="/static/styles.min.css";
```
