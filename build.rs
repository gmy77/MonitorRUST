// Incorpora l'icona nell'exe, così si vede in Esplora file e sulla barra delle applicazioni.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "Monitor Sensori");
        res.set("ProductName", "MonitorRUST");
        res.compile().expect("compilazione delle risorse Windows non riuscita");
    }
}
