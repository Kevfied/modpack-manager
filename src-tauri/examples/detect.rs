fn main() {
    let d = app_lib::launchers::detect();
    for l in &d.launchers {
        println!(
            "{} | {} | data={:?} | path={:?}",
            l.kind, l.name, l.data_dir, l.path
        );
    }
    println!("{} installed versions", d.installed_versions.len());
}
