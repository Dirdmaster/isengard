//! Build script: run `bun run build` in `web/` if web sources are newer than
//! `web/.output/public/index.html`. Skip silently if `bun` is not installed
//! (CI is expected to install it; local dev can build the bundle manually with
//! `bun run build` if needed).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let web_dir = manifest_dir.join("web");
    let output_index = web_dir.join(".output").join("public").join("index.html");

    // Tell cargo when to re-run this build script.
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/nuxt.config.ts");
    println!("cargo:rerun-if-changed=web/tailwind.config.ts");
    println!("cargo:rerun-if-changed=web/app.vue");
    rerun_dir(&web_dir.join("pages"));
    rerun_dir(&web_dir.join("components"));
    rerun_dir(&web_dir.join("composables"));
    rerun_dir(&web_dir.join("stores"));
    rerun_dir(&web_dir.join("assets"));

    // Skip if bun isn't installed. The bundle must already exist (CI built
    // it, or developer ran `bun run build` manually).
    let bun_available = Command::new("bun").arg("--version").output().is_ok();
    if !bun_available {
        if !output_index.exists() {
            panic!(
                "bun not found AND no pre-built bundle at {}. Install bun (https://bun.sh) or run `bun run build` in {}.",
                output_index.display(),
                web_dir.display()
            );
        }
        println!("cargo:warning=bun not found; using pre-built bundle at {}", output_index.display());
        return;
    }

    // Compare timestamps: skip rebuild if output is newer than all watched sources.
    if let Some(out_ts) = mtime(&output_index) {
        let needs_rebuild = walk_for_newer(&web_dir.join("pages"), out_ts)
            || walk_for_newer(&web_dir.join("components"), out_ts)
            || walk_for_newer(&web_dir.join("composables"), out_ts)
            || walk_for_newer(&web_dir.join("stores"), out_ts)
            || walk_for_newer(&web_dir.join("assets"), out_ts)
            || mtime(&web_dir.join("package.json")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("nuxt.config.ts")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("tailwind.config.ts")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("app.vue")).map_or(false, |t| t > out_ts);
        if !needs_rebuild {
            println!("cargo:warning=dashboard bundle up to date; skipping bun build");
            return;
        }
    }

    // Always install with --frozen-lockfile. Cheap when nothing changed
    // (bun checks the lockfile vs node_modules and exits in <1s if in sync).
    // Catches the case where package.json was updated but node_modules wasn't.
    run("bun", &["install", "--frozen-lockfile"], &web_dir);
    run("bun", &["run", "build"], &web_dir);

    // Verify output exists.
    if !output_index.exists() {
        panic!("bun run build completed but {} doesn't exist", output_index.display());
    }
}

fn rerun_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    // Cargo's rerun-if-changed on a directory is non-recursive. Emit one entry
    // per file in the subtree so deep edits trigger the build script.
    fn walk(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&entry.path());
            } else if ft.is_file() {
                println!("cargo:rerun-if-changed={}", entry.path().display());
            }
        }
    }
    walk(dir);
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn walk_for_newer(dir: &Path, ts: SystemTime) -> bool {
    if !dir.exists() {
        return false;
    }
    fn walk(dir: &Path, ts: SystemTime) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if walk(&path, ts) {
                    return true;
                }
            } else if ft.is_file() {
                if mtime(&path).map_or(false, |t| t > ts) {
                    return true;
                }
            }
        }
        false
    }
    walk(dir, ts)
}

fn run(cmd: &str, args: &[&str], cwd: &Path) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd} {}`: {e}", args.join(" ")));
    if !status.success() {
        panic!("`{cmd} {}` failed with {status}", args.join(" "));
    }
}
