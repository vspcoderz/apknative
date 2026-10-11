mod axml;
mod dex;
mod runtime;
mod verify;
mod zip;

use dex::Dex;
use runtime::Runtime;
use std::env;
use std::fs;
use std::process::ExitCode;

const USAGE: &str = "\
apknative — run Android apps without an emulator

USAGE:
  apknative run <file.apk|classes.dex> [--entry <Class.method>]
  apknative manifest <file.apk>            show AndroidManifest.xml
  apknative info <file.apk>
  apknative classes <file.apk>

NOTES:
  `run` executes a static main(String[]) if the DEX has one. Real APKs don't,
  so it falls back to the launcher Activity named in AndroidManifest.xml and
  invokes its onCreate(Bundle).
";

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    if argv.len() < 3 {
        print!("{USAGE}");
        return ExitCode::from(2);
    }
    let result = match argv[1].as_str() {
        "run" => cmd_run(&argv[2], &argv[3..]),
        "manifest" => cmd_manifest(&argv[2]),
        "info" => cmd_info(&argv[2]),
        "verify" => cmd_verify(&argv[2]),
        "classes" => cmd_classes(&argv[2]),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Open the target as either a raw DEX or an APK archive.
enum Source {
    Dex(Vec<u8>),
    Apk(zip::Zip),
}

fn open(path: &str) -> Result<Source, String> {
    let data = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if data.len() >= 4 && &data[0..4] == b"dex\n" {
        return Ok(Source::Dex(data));
    }
    let z = zip::Zip::open(data).map_err(|e| format!("{path}: {e}"))?;
    Ok(Source::Apk(z))
}

/// Pull the primary DEX out of a source.
fn primary_dex(src: &Source) -> Result<Vec<u8>, String> {
    Ok(all_dexes(src)?.remove(0).1)
}

/// Every DEX blob in the source, in load order. Real apps are usually multidex,
/// so a class we care about may not be in `classes.dex`.
fn all_dexes(src: &Source) -> Result<Vec<(String, Vec<u8>)>, String> {
    match src {
        Source::Dex(d) => Ok(vec![("<input>".to_string(), d.clone())]),
        Source::Apk(z) => {
            let names = z.names()?;
            let mut v = Vec::new();
            for n in names
                .iter()
                .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
            {
                v.push((n.clone(), z.read(n)?));
            }
            if v.is_empty() {
                return Err("archive contains no classes.dex".into());
            }
            Ok(v)
        }
    }
}

fn cmd_run(path: &str, rest: &[String]) -> Result<(), String> {
    let mut entry: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--entry" => {
                entry = rest.get(i + 1).cloned();
                i += 2;
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }

    let src = open(path)?;
    let raw_dexes = all_dexes(&src)?;
    let parsed: Vec<Dex> = raw_dexes
        .iter()
        .map(|(_, raw)| Dex::parse(raw.clone()))
        .collect::<Result<_, _>>()?;
    let n = parsed.len();
    let mut rt = Runtime::new(parsed);

    if let Some(spec) = entry {
        let (cls, meth) = spec
            .rsplit_once('.')
            .ok_or("--entry expects <Class.method>")?;
        let target =
            find_entry(&rt, cls, meth).ok_or(format!("no static method `{spec}` in DEX"))?;
        return rt.run_entry(target);
    }

    // A plain `main(String[])` wins if any DEX has one.
    if rt.find_main().is_some() {
        return rt.run_main();
    }

    // Otherwise this is a real Android app: start its launcher Activity.
    let manifest = match &src {
        Source::Apk(z) => z.read("AndroidManifest.xml")?,
        Source::Dex(_) => return Err("no `main(String[])` in this DEX".into()),
    };
    let root = axml::parse(&manifest)?;
    let activity = axml::launcher_activity(&root)
        .ok_or("no main(String[]) and no launcher activity in AndroidManifest.xml")?;

    println!("no main(); launching Activity: {activity}");
    if !rt.has_class(&activity) {
        return Err(format!(
            "class {activity} not found in any of the {n} DEX files"
        ));
    }
    if n > 1 {
        println!("({n} DEX files loaded)");
    }
    rt.run_activity(&activity)
}

fn cmd_manifest(path: &str) -> Result<(), String> {
    let src = open(path)?;
    let z = match src {
        Source::Apk(z) => z,
        Source::Dex(_) => return Err("not an APK (no archive)".into()),
    };
    let data = z.read("AndroidManifest.xml")?;
    let root = axml::parse(&data)?;
    let mut out = String::new();
    axml::dump(&root, 0, &mut out);
    print!("{out}");
    if let Some(a) = axml::launcher_activity(&root) {
        println!("\nlauncher activity: {a}");
    }
    Ok(())
}

fn cmd_verify(path: &str) -> Result<(), String> {
    let src = open(path)?;
    let dexes = all_dexes(&src)?;
    let mut total = 0usize;
    for (name, raw) in &dexes {
        let d = Dex::parse(raw.clone())?;
        let errs = verify::verify(&d);
        if !errs.is_empty() {
            println!("--- {name}: {} problem(s) ---", errs.len());
            for e in errs.iter().take(40) {
                println!("  {e}");
            }
            if errs.len() > 40 {
                println!("  ... and {} more", errs.len() - 40);
            }
        }
        total += errs.len();
    }
    if total == 0 {
        println!("no problems found");
        Ok(())
    } else {
        println!("total: {total} problem(s)");
        Ok(())
    }
}

fn find_entry(rt: &Runtime, cls: &str, meth: &str) -> Option<u32> {
    for cd in &rt.primary().classes {
        if rt.primary().class_name(cd.class_idx) != cls {
            continue;
        }
        let data = rt.primary().class_data(cd.class_data_off).ok()?;
        for em in data
            .direct_methods
            .iter()
            .chain(data.virtual_methods.iter())
        {
            if rt.primary().string(rt.primary().methods[em.method_idx as usize].name) == meth {
                return Some(em.method_idx);
            }
        }
    }
    None
}

fn cmd_info(path: &str) -> Result<(), String> {
    let src = open(path)?;
    let raw = primary_dex(&src)?;
    println!("file       {path}");
    println!("bytes      {}", raw.len());
    let d = Dex::parse(raw)?;
    println!("dex        {}", d.version);
    println!("strings    {}", d.strings.len());
    println!("types      {}", d.types.len());
    println!("protos     {}", d.protos.len());
    println!("fields     {}", d.fields.len());
    println!("methods    {}", d.methods.len());
    println!("classes    {}", d.classes.len());
    Ok(())
}

fn cmd_classes(path: &str) -> Result<(), String> {
    let src = open(path)?;
    let raw = primary_dex(&src)?;
    let d = Dex::parse(raw)?;
    for cd in &d.classes {
        let flags = match cd.access_flags & 0x000f {
            0x1 => "public",
            0x2 => "private",
            0x4 => "protected",
            _ => "",
        };
        println!("class {} {flags}", d.class_name(cd.class_idx));
        let data = d
            .class_data(cd.class_data_off)
            .map_err(|e| format!("{}: {e}", d.class_name(cd.class_idx)))?;
        for em in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
            let m = &d.methods[em.method_idx as usize];
            let ret = d.return_type(m);
            let params: Vec<&str> = d
                .proto(m.proto)
                .map(|p| p.params.iter().map(|&t| d.type_name(t)).collect())
                .unwrap_or_default();
            let tag = if em.access_flags & 0x0008 != 0 {
                "static"
            } else {
                "virtual"
            };
            println!(
                "    {tag} {} {}({})",
                d.string(m.name),
                ret,
                params.join(", ")
            );
        }
    }
    Ok(())
}