use anyhow::{anyhow, Result};
use elf::endian::{EndianParse, NativeEndian};
use elf::ElfStream;
use path_absolutize::Absolutize;
use std::collections::{HashMap, VecDeque};
use std::fs::{read_dir, read_link, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use std::{env, fs};

static DEFAULT_LDSO: OnceLock<PathBuf> = OnceLock::new();

fn main() {
    {
        let ldso = if cfg!(target_arch = "x86_64") {
            "/lib64/ld-linux-x86-64.so.2"
        } else if cfg!(target_arch = "aarch64") {
            "/lib/ld-linux-aarch64.so.1"
        } else {
            "/lib/ld-linux.so.2"
        };

        DEFAULT_LDSO.get_or_init(|| PathBuf::from_str(ldso).unwrap());
    }

    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("missing arguments, please provide at least one path to resolve!");
        exit(1);
    }

    let cwd = env::current_dir().expect("unable to determine current working directory");
    let mut unprocessed_paths =
        VecDeque::from_iter(args.into_iter().map(|s| resolve_path(&s, &cwd)));
    let mut processed_paths = HashMap::<PathBuf, bool>::new();

    while !unprocessed_paths.is_empty() {
        let path = unprocessed_paths.pop_front().unwrap();
        if !processed_paths.contains_key(&path) {
            match process(&path) {
                Ok(results) => {
                    let reference = if path.is_dir() {
                        path.as_ref()
                    } else {
                        path.parent().expect("no parent directory found")
                    };

                    for p in results.into_iter().filter_map(|p| {
                        let p = resolve_path(&p, reference);
                        if !processed_paths.contains_key(&p) {
                            Some(p)
                        } else {
                            None
                        }
                    }) {
                        unprocessed_paths.push_back(p);
                    }
                    processed_paths.insert(path, true);
                }
                Err(err) => {
                    eprintln!("warning: {} could not be resolved: {}", path.display(), err);
                    processed_paths.insert(path, false);
                }
            }
        }
    }

    let mut paths = processed_paths
        .into_iter()
        .filter_map(|(k, v)| {
            if v {
                Some(k.display().to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    paths.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

    for path in paths {
        println!("{}", path);
    }
}

fn process(path: &Path) -> Result<Vec<PathBuf>> {
    let metadata = fs::symlink_metadata(path)?;
    return if metadata.is_symlink() {
        Ok(vec![read_link(path)?])
    } else if metadata.is_dir() {
        Ok(read_dir(path)?
            .into_iter()
            .filter_map(|r| match r {
                Ok(r) => Some(r.path()),
                Err(_) => None,
            })
            .collect())
    } else if metadata.is_file() {
        process_file(path)
    } else {
        Err(anyhow!("path is unsupported"))
    };
}

fn process_file(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let mut file = File::options().read(true).open(path)?;
    let mut buffer = [0; 512];
    let bytes_read = file.read(&mut buffer)?;
    if let Some(shebang) = find_shebang(&buffer[0..bytes_read]) {
        let (cmd, mut args) = {
            let mut args = shebang.split_whitespace().collect::<VecDeque<_>>();
            if args.is_empty() {
                return Err(anyhow!("invalid shebang"));
            }
            let cmd = args.pop_front().unwrap();
            if args.is_empty() {
                (cmd, VecDeque::new())
            } else {
                (cmd, args)
            }
        };
        files.push(PathBuf::from_str(cmd).unwrap());
        if cmd == "/usr/bin/env" && !args.is_empty() {
            // handle special case
            files.push(which::which(args.pop_front().unwrap())?);
        }
    } else if infer::app::is_elf(&buffer[0..bytes_read]) {
        assert_eq!(file.seek(SeekFrom::Start(0))?, 0);
        files.extend(parse_elf(file, path)?.into_iter())
    }
    Ok(files)
}

fn parse_elf(file: File, path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let mut file = ElfStream::<NativeEndian, _>::open_stream(file)?;
    let ldso = match find_ldso(&mut file)? {
        Some(ldso) => ldso,
        None => DEFAULT_LDSO.get().unwrap().to_path_buf(),
    };
    drop(file);
    files.extend(find_elf_deps(&path, &ldso)?);
    files.push(ldso);
    Ok(files)
}

fn find_ldso<E: EndianParse, S: std::io::Read + std::io::Seek>(
    file: &mut ElfStream<E, S>,
) -> Result<Option<PathBuf>> {
    let interp_header = match file.section_header_by_name(".interp")? {
        Some(h) => *h,
        None => return Ok(None),
    };

    let interp_data = file.section_data(&interp_header)?;
    assert_eq!(interp_data.1, None, "compressed headers unsupported");
    let end = interp_data
        .0
        .iter()
        .position(|&c| c == b'\0')
        .unwrap_or(interp_data.0.len());
    let interp = String::from_utf8_lossy(&interp_data.0[..end]);

    if !interp.is_empty() {
        Ok(Some(PathBuf::from(interp.as_ref())))
    } else {
        Ok(None)
    }
}

fn find_elf_deps(path: &Path, ldso: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];

    let mut handle = Command::new(ldso)
        .arg(path)
        .env("LD_TRACE_LOADED_OBJECTS", "1")
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(stdout) = handle.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if let Some(position) = line.find("(0x") {
                let line = line[0..position].trim();
                if let Some((_, dep)) = line.split_once("=>") {
                    let dep = dep.trim();
                    if !dep.is_empty() {
                        files.push(PathBuf::from_str(dep).unwrap());
                    }
                }
            }
        }
    }

    Ok(files)
}

fn find_shebang(buffer: &[u8]) -> Option<&str> {
    if buffer.starts_with(b"#!") {
        // Find the end of the shebang line by looking for a newline character
        if let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            // Convert the shebang bytes to a &str, excluding the newline
            // and return it. Ignore UTF-8 parsing errors for simplicity.
            return std::str::from_utf8(&buffer[2..end]).ok();
        }
    }
    None
}

fn resolve_path<P1: AsRef<Path> + ?Sized, P2: AsRef<Path> + ?Sized>(
    path: &P1,
    reference: &P2,
) -> PathBuf {
    path.as_ref()
        .absolutize_from(reference.as_ref())
        .expect("unable to absolutize path")
        .to_path_buf()
}
