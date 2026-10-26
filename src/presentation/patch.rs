//! Linear, bounded whole-file Git patches; no diff algorithm can consume quadratic work.

use std::path::Path;

fn quoted(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    #[cfg(windows)]
    let normalized;
    #[cfg(windows)]
    let text = if path.is_absolute() {
        // Git accepts drive/UNC paths, not Win32's verbatim namespace returned by canonicalize.
        normalized = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            format!("//{}", rest.replace('\\', "/"))
        } else {
            text.strip_prefix(r"\\?\")
                .unwrap_or(text)
                .replace('\\', "/")
        };
        &normalized
    } else {
        text
    };
    let mut out = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            32..=126 => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('"');
    Some(out)
}

pub(crate) fn generate(
    operation: &str,
    path: &Path,
    source: Option<&Path>,
    old: Option<&str>,
    new: Option<&str>,
) -> Option<String> {
    generate_with_mode(operation, path, source, old, new, Some(0o100644))
}

pub(crate) fn generate_with_mode(
    operation: &str,
    path: &Path,
    source: Option<&Path>,
    old: Option<&str>,
    new: Option<&str>,
    mode: Option<u32>,
) -> Option<String> {
    let old_path = quoted(source.unwrap_or(path))?;
    let new_path = quoted(path)?;
    let mut patch = format!("diff --git {old_path} {new_path}\n");
    match operation {
        "move" | "copy" => {
            let verb = if operation == "move" {
                "rename"
            } else {
                "copy"
            };
            patch.push_str(&format!(
                "similarity index 100%\n{verb} from {old_path}\n{verb} to {new_path}\n"
            ));
            return encoded_bound(patch);
        }
        "add" => patch.push_str("new file mode 100644\n"),
        "delete" => patch.push_str(&format!("deleted file mode {:o}\n", mode?)),
        _ => {}
    }
    let (old, new) = match operation {
        "add" => ("", new?),
        "delete" => (old?, ""),
        _ => (old?, new?),
    };
    if old.len().saturating_add(new.len()) > super::PATCH_BYTES / 2 {
        return None;
    }
    if old.contains('\0') || new.contains('\0') {
        return None;
    }
    let before = if operation == "add" {
        "/dev/null"
    } else {
        &old_path
    };
    let after = if operation == "delete" {
        "/dev/null"
    } else {
        &new_path
    };
    patch.push_str(&format!("--- {before}\n+++ {after}\n"));
    let old_count = old.split_inclusive('\n').count();
    let new_count = new.split_inclusive('\n').count();
    if old_count + new_count > 0 {
        patch.push_str(&format!(
            "@@ -{},{old_count} +{},{new_count} @@\n",
            usize::from(old_count > 0),
            usize::from(new_count > 0)
        ));
        for (prefix, text) in [('-', old), ('+', new)] {
            for line in text.split_inclusive('\n') {
                patch.push(prefix);
                patch.push_str(line);
                if !line.ends_with('\n') {
                    patch.push_str("\n\\ No newline at end of file\n");
                }
            }
        }
    }
    encoded_bound(patch)
}

fn encoded_bound(patch: String) -> Option<String> {
    // Receipt reservations count retained JSON, not raw text: control characters can expand sixfold.
    let encoded = serde_json::to_vec(&patch).ok()?.len();
    (encoded.saturating_add(r#"{"format":"git_patch","text":}"#.len()) <= super::PATCH_BYTES)
        .then_some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_patches_apply_for_each_operation_without_path_prefixes_or_lost_newlines() {
        use std::io::Write;
        for operation in ["add", "modify", "delete", "move", "copy"] {
            let root = tempfile::tempdir().unwrap();
            // macOS exposes temporary directories through /var -> /private/var; git apply rejects symlink ancestors.
            #[cfg(unix)]
            let directory = std::fs::canonicalize(root.path()).unwrap();
            #[cfg(windows)]
            let directory = root.path().to_path_buf();
            let old_path = directory.join("source file");
            let path = if matches!(operation, "move" | "copy") {
                directory.join("destination file")
            } else {
                old_path.clone()
            };
            if operation != "add" {
                std::fs::write(&old_path, "old\nwithout newline").unwrap();
            }
            let patch = generate(
                operation,
                &path,
                Some(&old_path),
                Some("old\nwithout newline"),
                Some("new\r\nwithout newline"),
            )
            .unwrap();
            assert!(!patch.starts_with("diff --git \"a/"));
            assert!(
                !patch
                    .lines()
                    .any(|line| line.starts_with("--- \"a/") || line.starts_with("+++ \"b/"))
            );
            let mut child = std::process::Command::new("git")
                .args([
                    "-c",
                    "core.autocrlf=false",
                    "-c",
                    "core.eol=lf",
                    "apply",
                    "--whitespace=nowarn",
                    "--unsafe-paths",
                    "-p0",
                ])
                .current_dir(root.path())
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(patch.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{operation}: {}\n{patch}",
                String::from_utf8_lossy(&output.stderr)
            );
            if operation == "delete" {
                assert!(!path.exists());
            } else {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    if matches!(operation, "move" | "copy") {
                        b"old\nwithout newline".as_slice()
                    } else {
                        b"new\r\nwithout newline".as_slice()
                    }
                );
            }
        }
    }

    #[test]
    fn unusual_paths_are_git_quoted_and_large_or_binary_text_omits_the_patch() {
        let path = Path::new("/root/space \"quote\"\tline\n\\unicode-é");
        let patch = generate("modify", path, None, Some("before"), Some("after")).unwrap();
        assert!(patch.contains("\\\"quote\\\"\\011line\\012\\\\unicode-\\303\\251"));
        assert!(generate("modify", path, None, Some("\0"), Some("text")).is_none());
        assert!(
            generate(
                "modify",
                path,
                None,
                Some(&"x".repeat(super::super::PATCH_BYTES)),
                Some("text")
            )
            .is_none()
        );
        assert!(generate("modify", path, None, None, Some("text")).is_none());
    }

    #[test]
    fn json_expanding_text_cannot_exceed_the_reserved_encoded_patch_budget() {
        assert!(
            generate(
                "delete",
                Path::new("/root/file"),
                None,
                Some(&"\u{1}".repeat(super::super::PATCH_BYTES / 2)),
                None
            )
            .is_none()
        );
        let patch = generate(
            "modify",
            Path::new("/root/file"),
            None,
            Some("\u{1}"),
            Some("\t"),
        )
        .unwrap();
        assert!(serde_json::to_vec(&patch).unwrap().len() <= super::super::PATCH_BYTES);
    }

    #[test]
    fn deletion_patches_preserve_known_executable_modes_and_omit_unknown_modes() {
        let path = Path::new("/root/script");
        let patch = generate_with_mode(
            "delete",
            path,
            None,
            Some("#!/bin/sh\n"),
            None,
            Some(0o100755),
        )
        .unwrap();
        assert!(patch.contains("deleted file mode 100755\n"));
        assert!(generate_with_mode("delete", path, None, Some("script"), None, None).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_paths_use_absolute_git_drive_and_unc_notation() {
        assert_eq!(
            quoted(Path::new(r"\\?\C:\workspace\file")),
            Some("\"C:/workspace/file\"".into())
        );
        assert_eq!(
            quoted(Path::new(r"\\?\UNC\server\share\file")),
            Some("\"//server/share/file\"".into())
        );
    }
}
