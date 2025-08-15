use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy)]
enum FontKind { Otf, Ttf }

fn detect_kind(path: &Path) -> io::Result<FontKind> {
    // Try extension first (case-insensitive)
    if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
        match ext.as_str() {
            "otf" => return Ok(FontKind::Otf),
            "ttf" | "ttc" => return Ok(FontKind::Ttf),
            _ => {}
        }
    }
    // Fallback: sniff magic bytes
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;

    if &magic == b"OTTO" {
        return Ok(FontKind::Otf);
    }
    if magic == [0x00, 0x01, 0x00, 0x00] || &magic == b"true" || &magic == b"ttcf" {
        return Ok(FontKind::Ttf);
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown font format (not OTF/TTF)"))
}

fn unique_path(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("font");
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("");
    for i in 1.. {
        let candidate = if ext.is_empty() {
            dest.with_file_name(format!("{stem}-{i}"))
        } else {
            dest.with_file_name(format!("{stem}-{i}.{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn move_across_fs(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.raw_os_error() == Some(18) /* EXDEV */ {
                fs::copy(src, dst)?;
                fs::remove_file(src)?;
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn set_permissions644(path: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o644);
    fs::set_permissions(path, perms)
}

fn refresh_font_cache() {
    match Command::new("fc-cache").arg("-f").status() {
        Ok(status) if status.success() => {}
        Ok(_) => eprintln!("Warning: fc-cache returned non-zero status."),
        Err(_) => eprintln!("Warning: fc-cache not found. Install fontconfig or refresh cache manually."),
    }
}

fn user_fonts_base() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("fonts")
    } else if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/fonts")
    } else {
        PathBuf::from(".local/share/fonts")
    }
}

fn is_perm_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(13)
}

fn escalate_and_reexec() -> io::Result<()> {
    // Prevent loops if we’re already elevated
    if env::var_os("INSTALL_FONT_ELEVATED").is_some() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied even after sudo retry"));
    }

    let exe = env::current_exe()?;
    let args: Vec<String> = env::args().skip(1).collect();

    eprintln!("Permission denied. Retrying with sudo… (you may be prompted for your password)");
    let status = Command::new("sudo")
        .env("INSTALL_FONT_ELEVATED", "1")
        .arg(exe)
        .args(&args)
        .status();

    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to execute sudo: {e}")
        )),
    }
}

fn do_install(user_mode: bool, src_path: &Path) -> io::Result<()> {
    let kind = detect_kind(src_path)?;

    let base_dir = if user_mode {
        user_fonts_base()
    } else {
        PathBuf::from("/usr/share/fonts")
    };

    let subdir = match kind {
        FontKind::Otf => "OTF",
        FontKind::Ttf => "TTF",
    };
    let dest_dir = base_dir.join(subdir);

    fs::create_dir_all(&dest_dir)?;                      // may hit EACCES
    let file_name = src_path.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid source filename"))?;
    let dest_path = unique_path(dest_dir.join(file_name));

    move_across_fs(src_path, &dest_path)?;               // may hit EACCES
    set_permissions644(&dest_path)?;                     // may hit EACCES

    println!("Installed {} -> {}", src_path.display(), dest_path.display());
    refresh_font_cache();                                // not critical if it fails
    Ok(())
}

fn main() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.len() > 2 {
        eprintln!("Usage: install_font <path-to-font> [--user]");
        eprintln!("  --user   Install to ~/.local/share/fonts (XDG) instead of /usr/share/fonts");
        std::process::exit(2);
    }

    let user_mode = args.iter().any(|a| a == "--user");
    let src_path = PathBuf::from(&args[0]);

    if !src_path.exists() || !src_path.is_file() {
        eprintln!("Error: source file does not exist or is not a file: {}", src_path.display());
        std::process::exit(1);
    }

    match do_install(user_mode, &src_path) {
        Ok(()) => Ok(()),
        Err(e) if is_perm_denied(&e) && !user_mode => {
            // Auto-retry with sudo for system-wide installs
            let _ = escalate_and_reexec()?;
            Ok(()) // unreachable
        }
        Err(e) => Err(e),
    }
}
