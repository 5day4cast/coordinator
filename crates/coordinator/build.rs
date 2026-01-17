use minify_js::{minify, Session, TopLevelMode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let templates_dir = Path::new(&manifest_dir).join("src/templates");
    let output_dir = Path::new(&manifest_dir).join("../../crates/public_ui");

    if !templates_dir.exists() {
        println!("cargo:warning=Templates directory not found, skipping JS bundling");
        return;
    }

    // In debug mode, clean up old hashed bundles to avoid duplicates
    if !is_release_build() {
        clean_old_bundles(&output_dir);
    }

    // Rerun if any JS or CSS file in templates changes
    println!("cargo:rerun-if-changed={}", templates_dir.display());
    for entry in WalkDir::new(&templates_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "js" || ext == "css")
        })
    {
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }

    // Build public app bundle: shared + components + pages + layouts/base.js
    let public_dirs = vec!["shared", "components", "pages", "layouts"];
    // Build admin bundle: shared + admin
    let admin_dirs = vec!["shared", "admin"];

    let mut manifest = HashMap::new();

    match build_bundle(
        &templates_dir,
        &output_dir,
        "app",
        &public_dirs,
        Some("base.js"),
    ) {
        Ok((hash, has_content)) => {
            if has_content {
                manifest.insert("app".to_string(), hash);
            }
        }
        Err(e) => println!("cargo:warning=Failed to build app bundle: {}", e),
    }

    match build_bundle(&templates_dir, &output_dir, "admin", &admin_dirs, None) {
        Ok((hash, has_content)) => {
            if has_content {
                manifest.insert("admin".to_string(), hash);
            }
        }
        Err(e) => println!("cargo:warning=Failed to build admin bundle: {}", e),
    }

    // Bundle CSS from templates + base styles
    match build_css_bundle(&templates_dir, &output_dir, &public_dirs) {
        Ok(Some(hash)) => {
            manifest.insert("styles".to_string(), hash);
        }
        Ok(None) => {}
        Err(e) => println!("cargo:warning=Failed to build CSS bundle: {}", e),
    }

    if !manifest.is_empty() {
        let manifest_path = output_dir.join("asset-manifest.json");
        let manifest_json = serde_json_minimal(&manifest);
        let _ = fs::write(&manifest_path, manifest_json);
    }

    // Copy static files (loader.js, etc.) to output directory
    copy_static_files(&templates_dir, &output_dir);
}

fn copy_static_files(templates_dir: &Path, output_dir: &Path) {
    let static_dir = templates_dir.join("static");
    if !static_dir.exists() {
        return;
    }

    for entry in WalkDir::new(&static_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let src_path = entry.path();
        let relative_path = src_path.strip_prefix(&static_dir).unwrap();
        let dest_path = output_dir.join(relative_path);

        // Create parent directories if needed
        if let Some(parent) = dest_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Err(e) = fs::copy(src_path, &dest_path) {
            println!("cargo:warning=Failed to copy {:?}: {}", src_path, e);
        } else {
            println!("cargo:warning=Copied static file: {:?}", relative_path);
        }
    }
}

fn build_css_bundle(
    templates_dir: &Path,
    output_dir: &Path,
    source_dirs: &[&str],
) -> Result<Option<String>, String> {
    let mut combined_css = String::new();

    // Start with base styles.css if it exists
    let base_styles = output_dir.join("styles.css");
    if base_styles.exists() {
        let content = fs::read_to_string(&base_styles)
            .map_err(|e| format!("Failed to read base styles: {}", e))?;
        combined_css.push_str(&content);
        combined_css.push('\n');
    }

    // Collect CSS from template directories
    for source_dir in source_dirs {
        let dir_path = templates_dir.join(source_dir);
        if !dir_path.exists() {
            continue;
        }

        let mut css_files: Vec<_> = WalkDir::new(&dir_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "css"))
            .map(|e| e.path().to_path_buf())
            .collect();

        css_files.sort();

        for css_file in css_files {
            let content = fs::read_to_string(&css_file)
                .map_err(|e| format!("Failed to read {:?}: {}", css_file, e))?;

            if content.trim().is_empty() {
                continue;
            }

            let relative_path = css_file
                .strip_prefix(templates_dir)
                .unwrap_or(&css_file)
                .display()
                .to_string();

            combined_css.push_str(&format!("\n/* === {} === */\n", relative_path));
            combined_css.push_str(&content);
            combined_css.push('\n');
        }
    }

    if combined_css.trim().is_empty() {
        return Ok(None);
    }

    let minified = minify_css(&combined_css);
    let hash = hash_content(&minified);
    let short_hash = &hash[..8];

    fs::write(
        output_dir.join(format!("styles.{}.min.css", short_hash)),
        &minified,
    )
    .map_err(|e| format!("Failed to write CSS bundle: {}", e))?;

    fs::write(output_dir.join("styles.min.css"), &minified)
        .map_err(|e| format!("Failed to write dev CSS: {}", e))?;

    println!(
        "cargo:warning=Built styles -> styles.{}.min.css ({} bytes)",
        short_hash,
        minified.len()
    );

    Ok(Some(short_hash.to_string()))
}

fn is_release_build() -> bool {
    env::var("PROFILE").is_ok_and(|p| p == "release")
}

fn clean_old_bundles(output_dir: &Path) {
    if !output_dir.exists() {
        return;
    }

    // Remove old hashed bundle files (pattern: name.hash.min.js, name.hash.min.css)
    for entry in fs::read_dir(output_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Check if file matches hashed bundle pattern (e.g., app.a1b2c3d4.min.js)
        let parts: Vec<&str> = filename.split('.').collect();
        if parts.len() >= 4 {
            let is_hashed_bundle = (parts.len() == 4
                && parts[2] == "min"
                && (parts[3] == "js" || parts[3] == "css"))
                || (parts.len() == 5 && parts[2] == "min" && parts[3] == "js" && parts[4] == "map");

            // Verify second part looks like a hash (8 hex chars)
            let looks_like_hash =
                parts[1].len() == 8 && parts[1].chars().all(|c| c.is_ascii_hexdigit());

            if is_hashed_bundle && looks_like_hash {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn build_bundle(
    templates_dir: &Path,
    output_dir: &Path,
    bundle_name: &str,
    source_dirs: &[&str],
    layout_file: Option<&str>,
) -> Result<(String, bool), String> {
    let mut combined_source = String::new();
    let mut source_map_entries = Vec::new();
    let mut current_line = 1;
    let mut has_any_content = false;

    for source_dir in source_dirs {
        let dir_path = templates_dir.join(source_dir);
        if !dir_path.exists() {
            continue;
        }

        let mut js_files: Vec<_> = WalkDir::new(&dir_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "js"))
            .map(|e| e.path().to_path_buf())
            .collect();

        // Sort for deterministic order, but put layout file last
        js_files.sort_by(|a, b| {
            let a_is_layout = layout_file.is_some_and(|lf| a.file_name().is_some_and(|n| n == lf));
            let b_is_layout = layout_file.is_some_and(|lf| b.file_name().is_some_and(|n| n == lf));
            match (a_is_layout, b_is_layout) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => a.cmp(b),
            }
        });

        for js_file in js_files {
            let content = fs::read_to_string(&js_file)
                .map_err(|e| format!("Failed to read {:?}: {}", js_file, e))?;

            if content.trim().is_empty() {
                continue;
            }

            has_any_content = true;

            let relative_path = js_file
                .strip_prefix(templates_dir)
                .unwrap_or(&js_file)
                .display()
                .to_string();

            let file_marker = format!("\n// === {} ===\n", relative_path);
            let line_count = content.lines().count();

            source_map_entries.push(SourceMapEntry {
                source_file: relative_path,
                start_line: current_line,
                line_count,
            });

            combined_source.push_str(&file_marker);
            current_line += file_marker.lines().count();
            combined_source.push_str(&content);
            combined_source.push('\n');
            current_line += line_count + 1;
        }
    }

    if !has_any_content {
        return Ok((String::new(), false));
    }

    let minified_str = match try_minify(&combined_source) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "cargo:warning=Minification failed for {}, using unminified: {}",
                bundle_name, e
            );
            combined_source.clone()
        }
    };

    let hash = hash_content(&minified_str);
    let short_hash = &hash[..8];

    let bundle_filename = format!("{}.{}.min.js", bundle_name, short_hash);
    fs::write(output_dir.join(&bundle_filename), &minified_str)
        .map_err(|e| format!("Failed to write bundle: {}", e))?;

    if !is_release_build() {
        let source_map = generate_source_map(&source_map_entries);
        fs::write(
            output_dir.join(format!("{}.{}.min.js.map", bundle_name, short_hash)),
            source_map,
        )
        .map_err(|e| format!("Failed to write source map: {}", e))?;
    }

    fs::write(
        output_dir.join(format!("{}.min.js", bundle_name)),
        &minified_str,
    )
    .map_err(|e| format!("Failed to write dev bundle: {}", e))?;

    if !is_release_build() {
        fs::write(
            output_dir.join(format!("{}.debug.js", bundle_name)),
            &combined_source,
        )
        .map_err(|e| format!("Failed to write debug bundle: {}", e))?;
    }

    println!(
        "cargo:warning=Built {} -> {} ({} bytes)",
        bundle_name,
        bundle_filename,
        minified_str.len()
    );

    Ok((short_hash.to_string(), true))
}

struct SourceMapEntry {
    source_file: String,
    start_line: usize,
    line_count: usize,
}

fn generate_source_map(entries: &[SourceMapEntry]) -> String {
    let mut map = String::from("// Source Map - File Locations\n");
    for entry in entries {
        map.push_str(&format!(
            "// Lines {}-{}: {}\n",
            entry.start_line,
            entry.start_line + entry.line_count - 1,
            entry.source_file
        ));
    }
    map
}

fn try_minify(source: &str) -> Result<String, String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let source_owned = source.to_string();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let session = Session::new();
        let mut minified = Vec::new();
        match minify(
            &session,
            TopLevelMode::Global,
            source_owned.as_bytes(),
            &mut minified,
        ) {
            Ok(_) => String::from_utf8(minified).map_err(|e| format!("UTF-8: {}", e)),
            Err(e) => Err(format!("{:?}", e)),
        }
    }));

    match result {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("minifier panicked".to_string()),
    }
}

fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn minify_css(css: &str) -> String {
    let mut result = String::with_capacity(css.len());
    let mut in_comment = false;
    let mut chars = css.chars().peekable();

    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            }
            continue;
        }

        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
            continue;
        }

        if c.is_whitespace() {
            if !result.ends_with(|ch: char| {
                ch.is_whitespace() || ch == '{' || ch == ':' || ch == ';' || ch == ','
            }) {
                if let Some(&next) = chars.peek() {
                    if !matches!(next, '{' | '}' | ':' | ';' | ',') {
                        result.push(' ');
                    }
                }
            }
            continue;
        }

        result.push(c);
    }

    result
}

fn serde_json_minimal(map: &HashMap<String, String>) -> String {
    let mut json = String::from("{");
    let entries: Vec<_> = map.iter().collect();
    for (i, (key, value)) in entries.iter().enumerate() {
        json.push_str(&format!("\"{}\":\"{}\"", key, value));
        if i < entries.len() - 1 {
            json.push(',');
        }
    }
    json.push('}');
    json
}
