use anyhow::{anyhow, Result};
use elf::endian::{EndianParse, NativeEndian};
use elf::ElfStream;
use findshlibs::{IterationControl, SharedLibrary, TargetSharedLibrary};
use path_absolutize::Absolutize;
use regex::Regex;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{read_dir, read_link, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use std::{env, fs};

static DEFAULT_LDSO: OnceLock<PathBuf> = OnceLock::new();
static GLIBC_VERSION_RE: OnceLock<Regex> = OnceLock::new();

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
        let (cmd, args) = {
            let mut args = shlex::split(shebang).unwrap_or(vec![]);
            if args.is_empty() {
                return Err(anyhow!("invalid shebang"));
            }
            let cmd = args.remove(0);
            if args.is_empty() {
                (cmd, vec![])
            } else {
                (cmd, args)
            }
        };
        files.push(PathBuf::from(cmd.clone()));
        if cmd == "/usr/bin/env" && !args.is_empty() {
            // handle special case
            // we need to find the command from the arguments
            if let Some(cmd) = find_env_cmd(args.as_slice()) {
                files.push(which::which(cmd)?);
            }
        }
    } else if infer::app::is_elf(&buffer[0..bytes_read]) {
        assert_eq!(file.seek(SeekFrom::Start(0))?, 0);
        files.extend(parse_elf(file, path)?.into_iter())
    }
    Ok(files)
}

fn find_env_cmd(args: &[String]) -> Option<&String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with('-') {
            match arg.as_str() {
                "-i" | "--ignore-environment" | "-0" | "--null" | "-v" | "--debug" => {}
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string" if iter.len() > 0 => {
                    iter.next(); // Skip the argument following the option
                }
                _ => continue, // Unrecognized or standalone options
            }
        } else if !arg.contains('=') {
            return Some(arg); // Found COMMAND
        }
    }
    None // No COMMAND found
}

fn parse_elf(file: File, path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    let mut file = ElfStream::<NativeEndian, _>::open_stream(file)?;
    let ldso = match find_ldso(&mut file)? {
        Some(ldso) => ldso,
        None => DEFAULT_LDSO.get().unwrap().to_path_buf(),
    };
    drop(file);
    if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
        if file_name.starts_with("libc.so") || file_name.starts_with("libc-") {
            // ok, this seems to be a libc
            // find out if it's a glibc
            if let Ok(glibc_version) = glibc_version(&path) {
                files.extend(find_glibc_deps(&glibc_version)?);
            }
        }
    }
    files.extend(find_elf_deps(&path, &ldso)?);
    files.push(ldso);
    Ok(files)
}

fn glibc_version(path: &Path) -> Result<Version> {
    let output = Command::new(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("glibc returned with non-success exit code"));
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    let first_line = output_str
        .lines()
        .next()
        .ok_or(anyhow::anyhow!("invalid output"))?;

    let re = GLIBC_VERSION_RE
        .get_or_init(|| Regex::new(r"(?i)^GNU C Library.*version (\d+\.\d+(?:\.\d+)?)").unwrap());

    let captures = re
        .captures(first_line)
        .ok_or(anyhow::anyhow!("invalid output"))?;
    let version_str = captures
        .get(1)
        .ok_or(anyhow::anyhow!("invalid output"))?
        .as_str();

    Version::from_str(version_str).map_err(|_| anyhow::anyhow!("invalid output"))
}

fn find_glibc_deps(version: &Version) -> Result<Vec<PathBuf>> {
    let mut files = vec![];
    if let Ok(Some(libgcc)) = find_dyn_lib("libgcc_s.so.1") {
        files.push(libgcc);
    }
    if let Ok(Some(libidn2)) = find_dyn_lib("libidn2.so.0") {
        files.push(libidn2);
    }

    let nss_conf = PathBuf::from_str("/etc/nsswitch.conf")?;
    if nss_conf.is_file() {
        for handler in parse_nsswitch_conf(&nss_conf)? {
            let mut candidates = Vec::with_capacity(3);
            if let Some(minor) = version.minor() {
                candidates.push(format!(
                    "libnss_{}-{}.{}.so",
                    handler,
                    version.major(),
                    minor
                ));
            }
            candidates.push(format!("libnss_{}.so.{}", handler, version.major()));
            candidates.push(format!("libnss_{}.so", handler));
            for lib in candidates {
                if let Ok(Some(lib)) = find_dyn_lib(lib) {
                    files.push(lib);
                }
            }
        }
        files.push(nss_conf);
    }
    Ok(files)
}

fn find_dyn_lib<S: AsRef<str>>(name: S) -> Result<Option<PathBuf>> {
    let name = OsStr::new(name.as_ref());
    let _lib = unsafe { libloading::os::unix::Library::new(name) }?;
    let mut lib_path = None;
    TargetSharedLibrary::each(|l| {
        let path = PathBuf::from(l.name());
        if path.file_name() == Some(name) {
            lib_path = Some(path);
            IterationControl::Break
        } else {
            IterationControl::Continue
        }
    });
    Ok(lib_path)
}

fn parse_nsswitch_conf(path: &Path) -> Result<HashSet<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut handlers = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        // Split the line at a '#' character to ignore comments
        let line = line.split('#').next().unwrap_or("");
        let parts: Vec<&str> = line.split_whitespace().collect();
        for (i, part) in parts.iter().enumerate() {
            // Skip the first part as it is the service name (e.g., "passwd:", "group:")
            if i > 0 && !part.ends_with(':') {
                let handler = part.split('[').next().unwrap(); // Split at '[' to ignore options
                if !handler.is_empty() {
                    handlers.insert(handler.to_string());
                }
            }
        }
    }

    Ok(handlers)
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

#[derive(Debug, Eq)]
struct Version(u64, Option<u64>, Option<u64>);

impl Version {
    fn major(&self) -> u64 {
        self.0
    }

    fn minor(&self) -> Option<u64> {
        self.1
    }

    #[allow(dead_code)]
    fn revision(&self) -> Option<u64> {
        self.2
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1 && self.2 == other.2
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.0.cmp(&other.0) {
            Ordering::Equal => match self.1.unwrap_or(0).cmp(&other.1.unwrap_or(0)) {
                Ordering::Equal => self.2.unwrap_or(0).cmp(&other.2.unwrap_or(0)),
                other => other,
            },
            other => other,
        }
    }
}

impl FromStr for Version {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        let major = parts.get(0).ok_or(())?.parse::<u64>().map_err(|_| ())?;
        let minor = parts.get(1).and_then(|s| s.parse::<u64>().ok());
        let patch = parts.get(2).and_then(|s| s.parse::<u64>().ok());
        Ok(Version(major, minor, patch))
    }
}
