// Step-by-step bisect of inspect_jar on a single jar:
//   cargo run --example dbgjar -- <jar>
use std::io::{BufReader, Read, Seek};

fn main() {
    let path = std::env::args().nth(1).expect("usage: dbgjar <jar>");
    eprintln!("[1] open");
    let mut f = BufReader::new(std::fs::File::open(&path).unwrap());

    eprintln!("[2] hash");
    let mut s1 = sha1::Sha1::new();
    let mut s5 = sha2::Sha512::new();
    use sha1::Digest;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 { break; }
        s1.update(&buf[..n]);
        s5.update(&buf[..n]);
        total += n as u64;
    }
    eprintln!("    {total} bytes hashed");

    eprintln!("[3] rewind");
    f.seek(std::io::SeekFrom::Start(0)).unwrap();

    eprintln!("[4] zip open");
    let mut zip = zip::ZipArchive::new(f).unwrap();
    eprintln!("    entries={}", zip.len());

    eprintln!("[5] file_names");
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    eprintln!("    names={}", names.len());

    eprintln!("[6] by_name mods.toml");
    match zip.by_name("META-INF/mods.toml") {
        Ok(mut e) => {
            let mut s = String::new();
            e.read_to_string(&mut s).unwrap();
            eprintln!("    toml len={}", s.len());
            eprintln!("[7] toml parse");
            let v = toml::from_str::<toml::Value>(&s);
            eprintln!("    parsed={}", v.is_ok());
        }
        Err(e) => eprintln!("    no mods.toml: {e}"),
    }
    eprintln!("[8] done");
}
