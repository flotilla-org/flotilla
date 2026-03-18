use bollard::{cli, server::SessionService};

fn main() {
    let cli = cli::parse();
    let service = if let Some(root) = cli.runtime_root.clone() {
        SessionService::new(bollard::runtime::RuntimeLayout::new(root))
    } else {
        SessionService::discover()
    };
    match cli::execute(cli, &service) {
        Ok(Some(output)) => println!("{output}"),
        Ok(None) => {}
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}
