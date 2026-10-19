//! relsign — minisign-compatible keygen + signing used by scripts/release.ps1
//! when no `minisign`/`rsign` CLI is installed. Produces standard formats:
//! secret key box, public key box, and detached .minisig signatures.
//!
//!   relsign gen <pub-file> <key-file>     — new keypair, prints base64 pubkey
//!   relsign sign <key-file> <file> <sig>  — detached sign
//!   relsign pubkey <key-file>             — print base64 pubkey

use minisign::{KeyPair, PublicKey, SecretKey};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("gen") => {
            let kp = KeyPair::generate_unencrypted_keypair().unwrap();
            std::fs::write(&args[2], kp.pk.to_box().unwrap().into_string()).unwrap();
            std::fs::write(&args[3], kp.sk.to_box(None).unwrap().into_string()).unwrap();
            println!("{}", kp.pk.to_base64());
        }
        Some("sign") => {
            let sk = SecretKey::from_file(&args[2], None).unwrap();
            let pk = PublicKey::from_secret_key(&sk).unwrap();
            let data = std::fs::read(&args[3]).unwrap();
            let sig =
                minisign::sign(Some(&pk), &sk, std::io::Cursor::new(data), None, None)
                    .unwrap();
            std::fs::write(&args[4], sig.to_string()).unwrap();
        }
        Some("pubkey") => {
            let sk = SecretKey::from_file(&args[2], None).unwrap();
            println!("{}", PublicKey::from_secret_key(&sk).unwrap().to_base64());
        }
        _ => {
            eprintln!("usage: relsign gen|sign|pubkey …");
            std::process::exit(2);
        }
    }
}
