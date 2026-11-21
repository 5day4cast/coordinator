//! Bundles the browser assets that live beside the templates.
//!
//! Every `.js` and `.css` file under `src/templates` belongs to the component
//! beside it. This script minifies them (oxc for scripts, lightningcss for
//! styles), concatenates them into `OUT_DIR` and writes `assets.rs`, which
//! embeds each file with `include_bytes!` under a URL containing a hash of
//! its content. Nothing is written to the source tree; only the separately
//! built WASM package is still served from disk.

use std::{
    env,
    error::Error,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions, StyleSheet};
use sha2::{Digest, Sha256};

/// Directories whose scripts make up the public site's bundle, in load order.
const PUBLIC_DIRS: &[&str] = &["shared", "components", "fragments", "pages", "layouts"];
/// The public bundle's entry point; it runs after every other script.
const PUBLIC_ENTRY: &str = "base.js";
/// Fonts under `vendor/fonts`: the constant naming each and its file stem.
const FONTS: &[(&str, &str)] = &[
    ("BARLOW_CONDENSED_600", "barlow-condensed-600"),
    ("BARLOW_CONDENSED_700", "barlow-condensed-700"),
    ("BARLOW_CONDENSED_700_ITALIC", "barlow-condensed-700-italic"),
    ("PUBLIC_SANS", "public-sans"),
    ("VT323", "vt323"),
];

struct Asset {
    /// Rust constant naming the asset in `assets.rs`.
    constant: &'static str,
    /// File stem of the public URL, before the hash.
    stem: &'static str,
    extension: &'static str,
    content_type: &'static str,
    bytes: Vec<u8>,
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    let manifest = env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is missing")?;
    let output = env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?;
    let templates = Path::new(&manifest).join("src/templates");
    let output = Path::new(&output);

    let mut files = Vec::new();
    collect(&templates, &mut files)?;
    files.sort();
    // Directories too, so added or removed files trigger a rebuild.
    println!("cargo::rerun-if-changed={}", templates.display());
    for file in &files {
        println!("cargo::rerun-if-changed={}", file.display());
    }

    // htmx, vendored once for the workspace as published (see its README).
    let htmx = Path::new(&manifest).join("../../vendor/htmx/4.0.0/htmx.min.js");
    println!("cargo::rerun-if-changed={}", htmx.display());
    // Bulma, vendored the same way; served from this site rather than a CDN.
    let bulma = Path::new(&manifest).join("../../vendor/bulma/1.0.2/bulma.min.css");
    println!("cargo::rerun-if-changed={}", bulma.display());
    // The public pages' typefaces; styles.css names each as `url("font:<stem>")`.
    let fonts_dir = Path::new(&manifest).join("../../vendor/fonts");
    println!("cargo::rerun-if-changed={}", fonts_dir.display());
    let fonts = FONTS
        .iter()
        .map(|&(constant, stem)| {
            Ok(Asset {
                constant,
                stem,
                extension: "woff2",
                content_type: "font/woff2",
                bytes: fs::read(fonts_dir.join(format!("{stem}.woff2")))?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    let static_dir = templates.join("static");
    let mut assets = vec![
        Asset {
            constant: "ADMIN_THEME_JS",
            stem: "admin-theme",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            // The admin pages' one script: the theme, then the game form's helpers.
            bytes: private_scope(bundle_scripts(
                &[
                    templates.join("admin/theme.js"),
                    templates.join("admin/create_form.js"),
                ],
                "admin-theme",
            )?),
        },
        Asset {
            constant: "WEATHER_MAP_JS",
            stem: "weather-map",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: private_scope(bundle_scripts(
                &[templates.join("admin/weather_map.js")],
                "weather-map",
            )?),
        },
        Asset {
            constant: "APP_JS",
            stem: "app",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: private_scope(bundle_scripts(&public_scripts(&templates, &files)?, "app")?),
        },
        Asset {
            constant: "BULMA_CSS",
            stem: "bulma",
            extension: "css",
            content_type: "text/css; charset=utf-8",
            bytes: fs::read(&bulma)?,
        },
        Asset {
            constant: "STYLES_CSS",
            stem: "styles",
            extension: "css",
            content_type: "text/css; charset=utf-8",
            bytes: stylesheet(&static_dir.join("styles.css"), &files, &fonts)?,
        },
        // A worker's script must be a file of its own; see shared/wasm.js.
        Asset {
            constant: "LOGIN_WORKER_JS",
            stem: "login-worker",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: bundle_scripts(&[static_dir.join("login-worker.js")], "login-worker")?,
        },
        // Solves the sign-up proof of work; see components/modals/signup_pow.js.
        Asset {
            constant: "POW_WORKER_JS",
            stem: "pow-worker",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: bundle_scripts(
                &[
                    static_dir.join("sha256.js"),
                    static_dir.join("pow-worker.js"),
                ],
                "pow-worker",
            )?,
        },
        // The recovery page's only script; also shipped in its standalone copy.
        Asset {
            constant: "RECOVER_JS",
            stem: "recover",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: private_scope(bundle_scripts(&[static_dir.join("recover.js")], "recover")?),
        },
        Asset {
            constant: "HTMX_JS",
            stem: "htmx",
            extension: "js",
            content_type: "text/javascript; charset=utf-8",
            bytes: fs::read(&htmx)?,
        },
        Asset {
            constant: "USA_MAP_SVG",
            stem: "usa-map",
            extension: "svg",
            content_type: "image/svg+xml",
            bytes: fs::read(static_dir.join("usa-map.svg"))?,
        },
    ];
    assets.extend(fonts);

    let mut generated =
        String::from("// Generated by build.rs; edit the files under src/templates instead.\n");
    for asset in &assets {
        let file = format!("{}.{}", asset.stem, asset.extension);
        fs::write(output.join(&file), &asset.bytes)?;
        let url = url(asset);
        writeln!(
            generated,
            "pub const {constant}: Asset = Asset {{ url: {url:?}, content_type: {content_type:?}, \
             bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\")) }};",
            constant = asset.constant,
            content_type = asset.content_type,
        )?;
    }
    weatherstar(&manifest, output)?;

    let constants: Vec<_> = assets.iter().map(|asset| asset.constant).collect();
    writeln!(
        generated,
        "pub const ALL: &[Asset] = &[{}];",
        constants.join(", ")
    )?;
    fs::write(output.join("assets.rs"), generated)?;
    Ok(())
}

/// WeatherStar 4000+ as vendored: `weatherstar.rs` lists every file with its path under
/// `/weatherstar/` and its type, embedded as it is. Its pages name each other by these paths, so
/// they keep them, unhashed.
fn weatherstar(manifest: &std::ffi::OsStr, output: &Path) -> Result<(), Box<dyn Error>> {
    let root = Path::new(manifest).join("../../vendor/ws4kp/8.2.3");
    println!("cargo::rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(&root, &mut files)?;
    files.sort();
    let mut generated = String::from(
        "// Generated by build.rs from vendor/ws4kp.\npub const WEATHERSTAR: &[(&str, &str, &[u8])] = &[\n",
    );
    for file in &files {
        println!("cargo::rerun-if-changed={}", file.display());
        let path = file
            .strip_prefix(&root)?
            .to_str()
            .ok_or("a WeatherStar path is not UTF-8")?
            .replace('\\', "/");
        if path == "README.md" {
            continue;
        }
        let content_type = match file.extension().and_then(|extension| extension.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js" | "mjs") => "text/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("json") => "application/json",
            Some("txt") | None => "text/plain; charset=utf-8",
            Some("woff") => "font/woff",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("svg") => "image/svg+xml",
            Some("ico") => "image/x-icon",
            Some(other) => return Err(format!("WeatherStar file of unknown type: {other}").into()),
        };
        writeln!(
            generated,
            "    ({path:?}, {content_type:?}, include_bytes!({:?})),",
            fs::canonicalize(file)?.display()
        )?;
    }
    generated.push_str("];\n");
    fs::write(output.join("weatherstar.rs"), generated)?;
    Ok(())
}

/// `/assets/<stem>.<first 16 hex digits of its SHA-256>.<extension>`.
fn url(asset: &Asset) -> String {
    let digest = hex::encode(Sha256::digest(&asset.bytes));
    format!(
        "/assets/{}.{}.{}",
        asset.stem,
        &digest[..16],
        asset.extension
    )
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().is_some_and(|found| found == extension)
}

/// Scripts under `dirs`, one directory after another, each sorted by path.
fn scripts_in(templates: &Path, dirs: &[&str], files: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .flat_map(|dir| {
            let root = templates.join(dir);
            files
                .iter()
                .filter(move |file| file.starts_with(&root) && has_extension(file, "js"))
                .cloned()
        })
        .collect()
}

fn public_scripts(templates: &Path, files: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut scripts = scripts_in(templates, PUBLIC_DIRS, files);
    let entry = scripts
        .iter()
        .position(|file| file.file_name().is_some_and(|name| name == PUBLIC_ENTRY))
        .ok_or_else(|| format!("the public bundle needs its entry point {PUBLIC_ENTRY}"))?;
    let entry = scripts.remove(entry);
    scripts.push(entry);
    Ok(scripts)
}

/// Minify each script on its own and join them. Each is parsed as a classic
/// script, so its top-level names are not renamed and the scripts can use
/// each other's. A file the parser rejects fails the build.
fn bundle_scripts(files: &[PathBuf], name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bundle = String::new();
    for file in files {
        let source = fs::read_to_string(file)?;
        let code = minify_script(&source)
            .map_err(|error| format!("{name}.js: {}: {error}", file.display()))?;
        bundle.push_str(&code);
        // Semicolons keep one script's last statement apart from the next.
        bundle.push_str(";\n");
    }
    Ok(bundle.into_bytes())
}

/// Runs a bundle inside one function, so that its scripts share their
/// top-level names (the wallet session in shared/wasm.js above all) without
/// putting them on `window`. Only what a script assigns to `window` is global.
fn private_scope(bundle: Vec<u8>) -> Vec<u8> {
    let mut wrapped = b"(()=>{\n".to_vec();
    wrapped.extend(bundle);
    wrapped.extend(b"})();\n");
    wrapped
}

/// The one place this script calls oxc, whose 0.x API changes between
/// releases (the version is pinned in Cargo.toml).
fn minify_script(source: &str) -> Result<String, String> {
    use oxc::{
        allocator::Allocator,
        codegen::{Codegen, CodegenOptions},
        minifier::{Minifier, MinifierOptions},
        parser::Parser,
        span::SourceType,
    };

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::script()).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(error.to_string());
    }
    if parsed.fatal_error {
        return Err("the parser gave up".into());
    }
    let mut program = parsed.program;
    let minified = Minifier::new(MinifierOptions::default()).minify(&allocator, &mut program);
    Ok(Codegen::new()
        .with_options(CodegenOptions::minify())
        .with_scoping(minified.scoping)
        .build(&program)
        .code)
}

/// Global styles first, then every component's styles, minified as one sheet. Each
/// `"font:<stem>"` becomes that font's hashed URL.
fn stylesheet(base: &Path, files: &[PathBuf], fonts: &[Asset]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut css = fs::read_to_string(base)?;
    for file in files {
        if has_extension(file, "css") && file != base {
            css.push('\n');
            css.push_str(&fs::read_to_string(file)?);
        }
    }
    for font in fonts {
        css = css.replace(
            &format!("\"font:{}\"", font.stem),
            &format!("\"{}\"", url(font)),
        );
    }
    if let Some(at) = css.find("\"font:") {
        let name: String = css[at..].chars().take(40).collect();
        return Err(format!("styles.css names a font that isn't bundled: {name}").into());
    }
    let mut sheet = StyleSheet::parse(
        &css,
        ParserOptions {
            filename: "src/templates (combined CSS)".into(),
            ..ParserOptions::default()
        },
    )
    .map_err(|error| io::Error::other(format!("parse CSS: {error}")))?;
    sheet
        .minify(MinifyOptions::default())
        .map_err(|error| io::Error::other(format!("minify CSS: {error}")))?;
    let printed = sheet
        .to_css(PrinterOptions {
            minify: true,
            ..PrinterOptions::default()
        })
        .map_err(|error| io::Error::other(format!("print CSS: {error}")))?;
    Ok(printed.code.into_bytes())
}
