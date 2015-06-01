/*!
`cargo-script` is a Cargo subcommand designed to let people quickly and easily run Rust "scripts" which can make use of Cargo's package ecosystem.

Or, to put it in other words, it lets you write useful, but small, Rust programs without having to create a new directory and faff about with `Cargo.toml`.

As such, `cargo-script` does two major things:

1. Given a script, it extracts the embedded Cargo manifest and merges it with some sensible defaults.  This manifest, along with the source code, is written to a fresh Cargo package on-disk.

2. It caches the generated and compiled packages, regenerating them only if the script or its metadata have changed.
*/
#![feature(collections)]
#![feature(metadata_ext)]
#![feature(path_ext)]

extern crate clap;
extern crate env_logger;
#[macro_use] extern crate log;
extern crate rustc_serialize;
extern crate shaman;
extern crate toml;
extern crate regex;

/**
If this is set to `true`, the digests used for package IDs will be replaced with "stub" to make testing a bit easier.  Obviously, you don't want this `true` for release...
*/
const STUB_HASHES: bool = false;

mod consts;
mod error;
mod platform;
mod util;

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use error::{Blame, MainError};

type Result<T> = std::result::Result<T, MainError>;

#[derive(Debug)]
struct Args {
    script: Option<String>,
    args: Vec<String>,

    expr: bool,
    loop_: bool,
    count: bool,

    build_only: bool,
    clear_cache: bool,
    debug: bool,
    dep: Vec<String>,
    force: bool,
}

fn parse_args() -> Args {
    use clap::{App, Arg, SubCommand};
    let version = option_env!("CARGO_PKG_VERSION").unwrap_or("unknown");
    let about = r#"Compiles and runs "Cargoified Rust scripts"."#;

    // "const str array slice"
    macro_rules! csas {
        ($($es:expr),*) => {
            {
                const PIN: &'static [&'static str] = &[$($es),*];
                PIN
            }
        }
    }

    // We have to kinda lie about who we are for the output to look right...
    let m = App::new("cargo")
        .bin_name("cargo")
        .version(version)
        .about(about)
        .arg_required_else_help(true)
        .subcommand_required_else_help(true)
        .subcommand(SubCommand::new("script")
            .version(version)
            .about(about)
            .usage("cargo script [FLAGS OPTIONS] [--] <script> <args>...")
            .arg(Arg::with_name("script")
                .help("Script file (with or without extension) to execute.")
                .index(1)
            )
            .arg(Arg::with_name("args")
                .help("Additional arguments passed to the script.")
                .index(2)
                .multiple(true)
            )
            .arg(Arg::with_name("expr")
                .help("Execute <script> as a literal expression and display the result.")
                .long("expr")
                .conflicts_with_all(csas!["loop"])
                .requires("script")
            )
            .arg(Arg::with_name("loop")
                .help("Execute <script> as a literal closure once for each line from stdin.")
                .long("loop")
                .conflicts_with_all(csas!["expr"])
                .requires("script")
            )
            .arg(Arg::with_name("clear_cache")
                .help("Clears out the script cache.")
                .long("clear-cache")
            )
            .arg(Arg::with_name("count")
                .help("Invoke the loop closure with two arguments: line, and line number.")
                .long("count")
                .requires("loop")
            )
            .arg(Arg::with_name("build_only")
                .help("Build the script, but don't run it.")
                .long("build-only")
                .requires("script")
            )
            .arg(Arg::with_name("debug")
                .help("Build a debug executable, not an optimised one.")
                .long("debug")
                .requires("script")
            )
            .arg(Arg::with_name("dep")
                .help("Add an additional Cargo dependency.  Each SPEC can be either just the package name (which will assume the latest version) or a full `name=version` spec.")
                .long("dep")
                .takes_value(true)
                .multiple(true)
                .requires("script")
            )
            .arg(Arg::with_name("force")
                .help("Force the script to be rebuilt.")
                .long("force")
                .requires("script")
            )
        )
        .get_matches();

    let m = m.subcommand_matches("script").unwrap();

    Args {
        script: m.value_of("script").map(Into::into),
        args: m.values_of("args").unwrap_or(vec![]).into_iter()
            .map(Into::into).collect(),

        expr: m.is_present("expr"),
        loop_: m.is_present("loop"),
        count: m.is_present("count"),

        build_only: m.is_present("build_only"),
        clear_cache: m.is_present("clear_cache"),
        debug: m.is_present("debug"),
        dep: m.values_of("dep").unwrap_or(vec![]).into_iter()
            .map(Into::into).collect(),
        force: m.is_present("force"),
    }
}

fn main() {
    env_logger::init().unwrap();
    info!("starting");
    info!("args: {:?}", std::env::args().collect::<Vec<_>>());
    let mut stderr = &mut std::io::stderr();
    match try_main() {
        Ok(0) => (),
        Ok(code) => {
            std::process::exit(code);
        },
        Err(ref err) if err.is_human() => {
            writeln!(stderr, "error: {}", err).unwrap();
            std::process::exit(1);
        },
        Err(ref err) => {
            writeln!(stderr, "internal error: {}", err).unwrap();
            std::process::exit(1);
        }
    }
}

fn try_main() -> Result<i32> {
    let args = parse_args();
    info!("Arguments: {:?}", args);

    /*
    If we've been asked to clear the cache, do that *now*.  There are two reasons:

    1. Do it *before* we call `cache_action_for` such that this flag *also* acts as a synonym for `--force`.
    2. Do it *before* we start trying to read the input so that, later on, we can make `<script>` optional, but still supply `--clear-cache`.
    */
    if args.clear_cache {
        try!(clean_cache(0));

        // If we *did not* get a `<script>` argument, that's OK.
        if args.script.is_none() {
            // Just let the user know that we did *actually* run.
            println!("cargo script cache cleared.");
            return Ok(0);
        }
    }

    // Take the arguments and work out what our input is going to be.  Primarily, this gives us the content, a user-friendly name, and a cache-friendly ID.
    // These three are just storage for the borrows we'll actually use.
    let script_name: String;
    let script_path: PathBuf;
    let content: String;

    let input = match (args.script, args.expr, args.loop_) {
        (Some(script), false, false) => {
            let (path, mut file) = try!(find_script(script).ok_or("could not find script"));

            script_name = path.file_stem()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or("unknown".into());

            let mut body = String::new();
            try!(file.read_to_string(&mut body));

            let mtime = platform::file_last_modified(&file);

            script_path = try!(std::env::current_dir()).join(path);
            content = body;

            Input::File(&script_name, &script_path, &content, mtime)
        },
        (Some(expr), true, false) => {
            content = expr;
            Input::Expr(&content)
        },
        (Some(loop_), false, true) => {
            content = loop_;
            Input::Loop(&content, args.count)
        },
        (None, _, _) => try!(Err((Blame::Human, consts::NO_ARGS_MESSAGE))),
        _ => try!(Err((Blame::Human,
            "cannot specify both --expr and --loop")))
    };
    info!("input: {:?}", input);

    /*
    Sort out the dependencies.  We want to do a few things:

    - Sort them so that they hash consistently.
    - Check for duplicates.
    - Expand `pkg` into `pkg=*`.
    */
    let deps = {
        use std::collections::HashMap;
        use std::collections::hash_map::Entry::{Occupied, Vacant};

        let mut deps: HashMap<String, String> = HashMap::new();
        for dep in args.dep {
            // Append '=*' if it needs it.
            let dep = match dep.find('=') {
                Some(_) => dep,
                None => dep + "=*"
            };

            let mut parts = dep.splitn(2, '=');
            let name = parts.next().expect("dependency is missing name");
            let version = parts.next().expect("dependency is missing version");
            assert!(parts.next().is_none(), "dependency somehow has three parts?!");

            if name == "" {
                try!(Err((Blame::Human, "cannot have empty dependency package name")));
            }

            if version == "" {
                try!(Err((Blame::Human, "cannot have empty dependency version")));
            }

            match deps.entry(name.into()) {
                Vacant(ve) => {
                    ve.insert(version.into());
                },
                Occupied(oe) => {
                    // This is *only* a problem if the versions don't match.  We won't try to do anything clever in terms of upgrading or resolving or anything... exact match or go home.
                    let existing = oe.get();
                    if &version != existing {
                        try!(Err((Blame::Human,
                            format!("conflicting versions for dependency '{}': '{}', '{}'",
                                name, existing, version))));
                    }
                }
            }
        }

        // Sort and turn into a regular vec.
        let mut deps: Vec<(String, String)> = deps.into_iter().collect();
        deps.sort();
        deps
    };
    info!("deps: {:?}", deps);

    // Work out what to do.
    let (action, pkg_path, meta) = cache_action_for(&input, args.debug, deps);
    info!("action: {:?}", action);
    info!("pkg_path: {:?}", pkg_path);
    info!("meta: {:?}", meta);

    // Compile if we need it.
    if action == CacheAction::Compile || args.force {
        info!("compiling...");
        try!(compile(&input, &meta, &pkg_path));
    }

    // Once we're done, clean out old packages from the cache.  There's no point if we've already done a full clear, though.
    let _defer_clear = {
        // To get around partially moved args problems.
        let cc = args.clear_cache;
        util::Defer::<_, MainError>::defer(move || {
            if !cc {
                try!(clean_cache(consts::MAX_CACHE_AGE_MS));
            }
            Ok(())
        })
    };

    if args.build_only {
        return Ok(0);
    }

    // Run it!
    let exe_path = get_exe_path(&input, &pkg_path, &meta);

    // FIXME: exe_path may not exist which should be an internal error

    info!("executing {:?}", exe_path);
    Ok(try!(Command::new(exe_path).args(&args.args).status()
        .map(|st| st.code().unwrap_or(1))))
}

/**
Clean up the cache folder.

Looks for all folders whose metadata says they were created at least `max_age` in the past and kills them dead.
*/
fn clean_cache(max_age: u64) -> Result<()> {
    info!("cleaning cache with max_age: {:?}", max_age);

    let cutoff = platform::current_time() - max_age;
    info!("cutoff:     {:>20?} ms", cutoff);

    let cache_dir = try!(get_cache_path());
    for child in try!(fs::read_dir(cache_dir)) {
        let child = try!(child);
        let path = child.path();
        if path.is_file() { continue }

        info!("checking: {:?}", path);

        let remove_dir = || {
            /*
            Ok, so *why* aren't we using `modified in the package metadata?  The point of *that* is to track what we know about the input.  The problem here is that `--expr` and `--loop` don't *have* modification times; they just *are*.

            Now, `PackageMetadata` *could* be modified to store, say, the moment in time the input was compiled, but then we couldn't use that field for metadata matching when decided whether or not a *file* input should be recompiled.

            So, instead, we're just going to go by the timestamp on the metadata file *itself*.
            */
            let meta_mtime = {
                let meta_path = get_pkg_metadata_path(&path);
                let meta_file = match fs::File::open(&meta_path) {
                    Ok(file) => file,
                    Err(..) => {
                        info!("couldn't open metadata for {:?}", path);
                        return true
                    }
                };
                platform::file_last_modified(&meta_file)
            };
            info!("meta_mtime: {:>20?} ms", meta_mtime);

            (meta_mtime <= cutoff)
        };

        if remove_dir() {
            info!("removing {:?}", path);
            if let Err(err) = fs::remove_dir_all(&path) {
                error!("failed to remove {:?} from cache: {}", path, err);
            }
        }
    }
    info!("done cleaning cache.");
    Ok(())
}

/**
Compile a package from the input.

Why take `PackageMetadata`?  To ensure that any information we need to depend on for compilation *first* passes through `cache_action_for` *and* is less likely to not be serialised with the rest of the metadata.
*/
fn compile<P>(input: &Input, meta: &PackageMetadata, pkg_path: P) -> Result<()>
where P: AsRef<Path> {
    let pkg_path = pkg_path.as_ref();

    let (mani_str, script_str) = try!(split_input(input, &meta.deps));

    try!(fs::create_dir_all(pkg_path));
    let cleanup_dir = util::Defer::defer(|| {
        fs::remove_dir_all(pkg_path)
    });

    let mani_path = {
        let mani_path = pkg_path.join("Cargo.toml");
        let mut mani_f = try!(fs::File::create(&mani_path));
        try!(write!(&mut mani_f, "{}", mani_str));
        try!(mani_f.flush());
        mani_path
    };

    let script_path = pkg_path.join(input.safe_name()).with_extension("rs");

    // If there is no main, then we can do some rust-doc-esque wrapping
    if !script_str.contains("fn main") {
        let lib_names = try!(extract_lib_names(&script_path,
                                               &mani_path,
                                               input.safe_name()));

        try!(write_script_with_externs(&script_str, lib_names, &script_path));

        // Seems like cargo wont recompile if we issue another build instantly
        thread::sleep_ms(10000);
    } else {
        let mut script_f = try!(fs::File::create(script_path));
        try!(write!(&mut script_f, "{}", script_str));
        try!(script_f.flush());
    }

    try!(cargo_build(meta, &mani_path));

    // Write out metadata *now*.  Remember that we check the timestamp in the metadata, *not* on the executable.
    try!(write_pkg_metadata(pkg_path, meta));

    cleanup_dir.disarm();
    Ok(())
}

fn cargo_build(meta: &PackageMetadata, mani_path: &Path) -> Result<()> {
    // *bursts through wall* It's Cargo Time!
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&*mani_path.to_string_lossy());

    if !meta.debug {
        cmd.arg("--release");
    }

    cmd.status().map_err(|e| Into::<MainError>::into(e)).and_then(|st|
        match st.code() {
            Some(0) => Ok(()),
            Some(st) => Err(format!("cargo failed with status {}", st).into()),
            None => Err("cargo failed".into())
    })
}

fn capture_cargo_build(mani_path: &Path) -> Result<String> {
    // *bursts through wall* It's Cargo Time!
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--verbose")
        .arg("--manifest-path")
        .arg(&*mani_path.to_string_lossy());

    cmd.output().map_err(|e| Into::<MainError>::into(e)).and_then(|output|
        match output.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&*output.stdout).to_string()),
            Some(st) => Err(format!("cargo failed with status {}", st).into()),
            None => Err("cargo failed".into())
    })
}

fn write_script_with_externs(script_str: &str, lib_names: Vec<String>, script_path: &Path) -> Result<()> {
    let mut script_f = try!(fs::File::create(&script_path));

    for lib_name in lib_names {
        try!(write!(&mut script_f, "extern crate {};\n", lib_name))
    }

    try!(write!(&mut script_f, "\nfn main() {{\n{}\n}}", script_str.trim()));
    try!(script_f.flush());
    Ok(())
}

fn extract_lib_names(script_path: &Path, mani_path: &Path, crate_name: &str) -> Result<Vec<String>> {
    // Write a dummy script
    let mut script_f = try!(fs::File::create(script_path));
    try!(write!(&mut script_f, "fn main() {{}}"));
    try!(script_f.flush());

    // Compile to capture `--extern`s provided by cargo
    let stdout = try!(capture_cargo_build(&mani_path));

    // FIXME: This should be more robust and match on the scriptname
    let regex = format!("--crate-name {}(.*)`", crate_name);
    let regex = regex::Regex::new(&regex).unwrap();

    let rustc_cmd = match regex.captures(&stdout) {
        Some(cap) => cap.at(1).unwrap(),
        None => {
            // This would probably be a failed dependency compile but that
            // should be handled by a statuscode of non-zero from `cargo build`
            unreachable!()
        }
    };

    // Extract libnames from `--extern`s
    let regex = regex::Regex::new(r#"--extern (.+?)="#).unwrap();
    let lib_names = regex.captures_iter(&rustc_cmd)
                         .map(|cap| cap.at(1).unwrap())
                         .map(|s| s.to_owned())
                         .collect();
    Ok(lib_names)
}

/**
Splits input into a complete Cargo manifest and unadultered Rust source.
*/
fn split_input(input: &Input, deps: &[(String, String)]) -> Result<(String, String)> {
    let (part_mani, source, template) = match *input {
        Input::File(_, _, content, _) => {
            /*
            We need to parse any partial manifest embedded in the content.  The only problem with this is that we *will not* assume the input is correctly formed, or that we've been passed a file that even *has* an embedded manifest; *i.e.* we might have been run with a plain Rust source file.

            First, we look for and discard a hashbang, if present.

            Next, we look for something which indicates the end of the embedded manifest.  *Officially*, this is a line which contains nothing but whitespace and *at least* three hyphens.  In *truth*, we will also look for anything that looks like Rust code.

            Specifically, we check for a line starting with any of the strings in `SPLIT_MARKERS`.  This should *hopefully* cover every possible valid Rust program.

            Once we've done that, we just chop the script content up in the appropriate places.
            */
            let mut lines = content.lines_any().peekable();

            let skip = if let Some(line) = lines.peek() {
                if line.starts_with("#!") && !line.starts_with("#![") {
                    // This is a hashbang; toss it.
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if skip { lines.next(); }

            let mut manifest_end = None;
            let mut source_start = None;

            for line in lines {
                // Did we get a dash separator?
                let mut dashes = 0;
                if line.chars().all(|c| {
                    if c == '-' { dashes += 1 }
                    c.is_whitespace() || c == '-'
                }) && dashes >= 3 {
                    info!("splitting because of dash divider in line {:?}", line);
                    manifest_end = Some(&line[0..0]);
                    source_start = Some(&line[line.len()..]);
                    break;
                }

                // Ok, it's-a guessin' time!  Yes, this is *evil*.
                const SPLIT_MARKERS: &'static [&'static str] = &[
                    "//", "/*", "#![", "#[", "pub ",
                    "extern ", "use ", "mod ", "type ",
                    "struct ", "enum ", "fn ", "impl ", "impl<",
                    "static ", "const ",
                ];

                let line_trimmed = line.trim_left();

                for marker in SPLIT_MARKERS {
                    if line_trimmed.starts_with(marker) {
                        info!("splitting because of marker '{:?}'", marker);
                        manifest_end = Some(&line[0..]);
                        source_start = Some(&line[0..]);
                        break;
                    }
                }
            }

            let (manifest, source) = match (manifest_end, source_start) {
                (Some(me), Some(ss)) => {
                    (&content[..content.subslice_offset(me)],
                        &content[content.subslice_offset(ss)..])
                },
                _ => try!(Err("could not locate start of Rust source in script"))
            };

            // Hooray!
            (manifest, source, consts::FILE_TEMPLATE)
        },
        Input::Expr(content) => ("", content, consts::EXPR_TEMPLATE),
        Input::Loop(content, count) => {
            let templ = if count { consts::LOOP_COUNT_TEMPLATE } else { consts::LOOP_TEMPLATE };
            ("", content, templ)
        },
    };

    let source = template.replace("%%", source);

    info!("part_mani: {:?}", part_mani);
    info!("source: {:?}", source);

    let part_mani = try!(toml::Parser::new(part_mani).parse()
        .ok_or("could not parse embedded manifest"));
    info!("part_mani: {:?}", part_mani);

    // It's-a mergin' time!
    let def_mani = try!(default_manifest(input));
    let dep_mani = try!(deps_manifest(deps));

    let mani = try!(merge_manifest(def_mani, part_mani));
    let mani = try!(merge_manifest(mani, dep_mani));
    info!("mani: {:?}", mani);

    let mani_str = format!("{}", toml::Value::Table(mani));
    info!("mani_str: {}", mani_str);

    Ok((mani_str, source))
}

/**
Generates a default Cargo manifest for the given input.
*/
fn default_manifest(input: &Input) -> Result<toml::Table> {
    let mani_str = consts::DEFAULT_MANIFEST.replace("%n", input.safe_name());
    toml::Parser::new(&mani_str).parse()
        .ok_or("could not parse default manifest, somehow".into())
}

/**
Generates a partial Cargo manifest containing the specified dependencies.
*/
fn deps_manifest(deps: &[(String, String)]) -> Result<toml::Table> {
    let mut mani_str = String::new();
    mani_str.push_str("[dependencies]\n");

    for &(ref name, ref ver) in deps {
        mani_str.push_str(name);
        mani_str.push_str("=");

        // We only want to quote the version if it *isn't* a table.
        let quotes = match ver.starts_with("{") { true => "", false => "\"" };
        mani_str.push_str(quotes);
        mani_str.push_str(ver);
        mani_str.push_str(quotes);
        mani_str.push_str("\n");
    }

    toml::Parser::new(&mani_str).parse()
        .ok_or("could not parse dependency manifest".into())
}

/**
Given two Cargo manifests, merges the second *into* the first.

Note that the "merge" in this case is relatively simple: only *top-level* tables are actually merged; everything else is just outright replaced.
*/
fn merge_manifest(mut into_t: toml::Table, from_t: toml::Table) -> Result<toml::Table> {
    for (k, v) in from_t {
        match v {
            toml::Value::Table(from_t) => {
                use std::collections::btree_map::Entry::*;

                // Merge.
                match into_t.entry(k) {
                    Vacant(e) => {
                        e.insert(toml::Value::Table(from_t));
                    },
                    Occupied(e) => {
                        let into_t = try!(as_table_mut(e.into_mut())
                            .ok_or((Blame::Human, "cannot merge manifests: cannot merge \
                                table and non-table values")));
                        into_t.extend(from_t);
                    }
                }
            },
            v => {
                // Just replace.
                into_t.insert(k, v);
            },
        }
    }

    return Ok(into_t);

    fn as_table_mut(t: &mut toml::Value) -> Option<&mut toml::Table> {
        match *t {
            toml::Value::Table(ref mut t) => Some(t),
            _ => None
        }
    }
}

/**
This represents what to do with the input provided by the user.
*/
#[derive(Clone, Debug, Eq, PartialEq)]
enum CacheAction {
    /// Compile the input into a fresh executable.
    Compile,
    /// Don't bother compiling; just execute what's in the cache.
    Execute,
}

/**
The metadata here serves two purposes:

1. It records everything necessary for compilation and execution of a package.
2. It records everything that must be exactly the same in order for a cached executable to still be valid, in addition to the content hash.
*/
#[derive(Clone, Debug, Eq, PartialEq, RustcDecodable, RustcEncodable)]
struct PackageMetadata {
    /// Path to the script file.
    path: Option<String>,

    /// Last-modified timestamp for script file.
    modified: Option<u64>,

    /// Was the script compiled in debug mode?
    debug: bool,

    /// Sorted list of dependencies.
    deps: Vec<(String, String)>,
}

/**
For the given input, this constructs the package metadata and checks the cache to see what should be done.
*/
fn cache_action_for(input: &Input, debug: bool, deps: Vec<(String, String)>) -> (CacheAction, PathBuf, PackageMetadata) {
    use std::fs::PathExt;

    // This can't fail.  Seriously, we're *fucked* if we can't work this out.
    let cache_path = get_cache_path().unwrap();
    info!("cache_path: {:?}", cache_path);

    let id = {
        let deps_iter = deps.iter()
            .map(|&(ref n, ref v)| (n as &str, v as &str));

        // Again, also fucked if we can't work this out.
        input.compute_id(deps_iter).unwrap()
    };
    info!("id: {:?}", id);

    let pkg_path = cache_path.join(&id);
    info!("pkg_path: {:?}", pkg_path);

    // Construct input metadata.
    let input_meta = {
        let (path, mtime) = match *input {
            Input::File(_, path, _, mtime)
                => (Some(path.to_string_lossy().into_owned()), Some(mtime)),
            Input::Expr(..)
            | Input::Loop(..)
                => (None, None)
        };
        PackageMetadata {
            path: path,
            modified: mtime,
            debug: debug,
            deps: deps,
        }
    };
    info!("input_meta: {:?}", input_meta);

    // Lazy powers, ACTIVATE!
    macro_rules! bail {
        () => {
            return (CacheAction::Compile, pkg_path, input_meta)
        }
    }

    let cache_meta = match get_pkg_metadata(&pkg_path) {
        Ok(meta) => meta,
        Err(err) => {
            info!("recompiling because: failed to load metadata");
            debug!("get_pkg_metadata error: {}", err.description());
            bail!()
        }
    };

    if cache_meta != input_meta {
        info!("recompiling because: metadata did not match");
        debug!("input metadata: {:?}", input_meta);
        debug!("cache metadata: {:?}", cache_meta);
        bail!()
    }

    // Next test: does the executable exist at all?
    let exe_path = get_exe_path(input, &pkg_path, &input_meta);
    if !exe_path.is_file() {
        info!("recompiling because: executable doesn't exist or isn't a file");
        bail!()
    }

    // That's enough; let's just go with it.
    (CacheAction::Execute, pkg_path, input_meta)
}

/**
Figures out where the output executable for the input should be.

Note that this depends on Cargo *not* suddenly changing its mind about where stuff lives.  In theory, I should be able to just *ask* Cargo for this information, but damned if I can't find an easy way to do it...
*/
fn get_exe_path<P>(input: &Input, pkg_path: P, meta: &PackageMetadata) -> PathBuf
where P: AsRef<Path> {
    let profile = match meta.debug {
        true => "debug",
        false => "release"
    };
    let mut exe_path = pkg_path.as_ref().join("target").join(profile).join(&input.safe_name()).into_os_string();
    exe_path.push(std::env::consts::EXE_SUFFIX);
    exe_path.into()
}

/**
Load the package metadata, given the path to the package's cache folder.
*/
fn get_pkg_metadata<P>(pkg_path: P) -> Result<PackageMetadata>
where P: AsRef<Path> {
    let meta_path = get_pkg_metadata_path(pkg_path);
    debug!("meta_path: {:?}", meta_path);
    let mut meta_file = try!(fs::File::open(&meta_path));

    let meta_str = {
        let mut s = String::new();
        meta_file.read_to_string(&mut s).unwrap();
        s
    };
    let meta: PackageMetadata = try!(rustc_serialize::json::decode(&meta_str)
        .map_err(|err| err.to_string()));

    Ok(meta)
}

/**
Work out the path to a package's metadata file.
*/
fn get_pkg_metadata_path<P>(pkg_path: P) -> PathBuf
where P: AsRef<Path> {
    pkg_path.as_ref().join(consts::METADATA_FILE)
}

/**
Save the package metadata, given the path to the package's cache folder.
*/
fn write_pkg_metadata<P>(pkg_path: P, meta: &PackageMetadata) -> Result<()>
where P: AsRef<Path> {
    let meta_path = get_pkg_metadata_path(pkg_path);
    debug!("meta_path: {:?}", meta_path);
    let mut meta_file = try!(fs::File::create(&meta_path));
    let meta_str = try!(rustc_serialize::json::encode(meta)
        .map_err(|err| err.to_string()));
    try!(write!(&mut meta_file, "{}", meta_str));
    try!(meta_file.flush());
    Ok(())
}

/**
Returns the path to the cache directory.
*/
fn get_cache_path() -> Result<PathBuf> {
    let cache_path = try!(platform::get_cache_dir_for("Cargo"));
    Ok(cache_path.join("script-cache"))
}

/**
Attempts to locate the script specified by the given path.  If the path as-given doesn't yield anything, it will try adding file extensions.
*/
fn find_script<P>(path: P) -> Option<(PathBuf, fs::File)>
where P: AsRef<Path> {
    let path = path.as_ref();

    // Try the path directly.
    if let Ok(file) = fs::File::open(path) {
        return Some((path.into(), file));
    }

    // If it had an extension, don't bother trying any others.
    if path.extension().is_some() {
        return None;
    }

    // Ok, now try other extensions.
    for &ext in consts::SEARCH_EXTS {
        let path = path.with_extension(ext);
        if let Ok(file) = fs::File::open(&path) {
            return Some((path, file));
        }
    }

    // Welp. ¯\_(ツ)_/¯
    None
}

/**
Represents an input source for a script.
*/
#[derive(Clone, Debug)]
enum Input<'a> {
    /**
    The input is a script file.

    The tuple members are: the name, absolute path, script contents, last modified time.
    */
    File(&'a str, &'a Path, &'a str, u64),

    /**
    The input is an expression.

    The tuple member is: the script contents.
    */
    Expr(&'a str),

    /**
    The input is a loop expression.

    The tuple member is: the script contents, whether the `--count` flag was given.
    */
    Loop(&'a str, bool),
}

impl<'a> Input<'a> {
    /**
    Return the "safe name" for the input.  This should be filename-safe.

    Currently, nothing is done to ensure this, other than hoping *really hard* that we don't get fed some excessively bizzare input filename.
    */
    pub fn safe_name(&self) -> &str {
        use Input::*;

        match *self {
            File(name, _, _, _) => name,
            Expr(..) => "expr",
            Loop(..) => "loop",
        }
    }

    /**
    Compute the package ID for the input.  This is used as the name of the cache folder into which the Cargo package will be generated.
    */
    pub fn compute_id<'dep, DepIt>(&self, deps: DepIt) -> Result<OsString>
    where DepIt: IntoIterator<Item=(&'dep str, &'dep str)> {
        use shaman::digest::Digest;
        use shaman::sha1::Sha1;
        use Input::*;

        let hash_deps = || {
            let mut hasher = Sha1::new();
            for dep in deps {
                hasher.input_str("dep=");
                hasher.input_str(dep.0);
                hasher.input_str("=");
                hasher.input_str(dep.1);
                hasher.input_str(";");
            }
            hasher
        };

        match *self {
            File(name, path, _, _) => {
                let mut hasher = Sha1::new();

                // Hash the path to the script.
                hasher.input_str(&path.to_string_lossy());
                let mut digest = hasher.result_str();
                digest.truncate(consts::ID_DIGEST_LEN_MAX);

                let mut id = OsString::new();
                id.push("file-");
                id.push(name);
                id.push("-");
                id.push(if STUB_HASHES { "stub" } else { &*digest });
                Ok(id)
            },
            Expr(content) => {
                let mut hasher = hash_deps();

                hasher.input_str(&content);
                let mut digest = hasher.result_str();
                digest.truncate(consts::ID_DIGEST_LEN_MAX);

                let mut id = OsString::new();
                id.push("expr-");
                id.push(if STUB_HASHES { "stub" } else { &*digest });
                Ok(id)
            },
            Loop(content, count) => {
                let mut hasher = hash_deps();

                // Make sure to include the [non-]presence of the `--count` flag in the flag, since it changes the actual generated script output.
                hasher.input_str("count:");
                hasher.input_str(if count { "true;" } else { "false;" });

                hasher.input_str(&content);
                let mut digest = hasher.result_str();
                digest.truncate(consts::ID_DIGEST_LEN_MAX);

                let mut id = OsString::new();
                id.push("loop-");
                id.push(if STUB_HASHES { "stub" } else { &*digest });
                Ok(id)
            },
        }
    }
}
