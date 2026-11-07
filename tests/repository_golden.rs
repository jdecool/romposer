//! `show`/`search` against composer 2.11.1 on a recorded Packagist served locally,
//! plus one live `show symfony/console --available --format=json`.

mod common;

use common::{composer, romposer, run, temp_dir};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

const LAST_MODIFIED: &str = "Wed, 01 Jan 2025 00:00:00 GMT";

/// Serves the recorded fixtures by request target (keep-alive, one thread per
/// connection), 304 on a matching If-Modified-Since.
fn serve_fixtures(log: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/repo");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let log = log.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        return;
                    }
                    let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    let mut not_modified = false;
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                            break;
                        }
                        if header.to_lowercase().starts_with("if-modified-since:") {
                            not_modified = header[18..].trim() == LAST_MODIFIED;
                        }
                    }
                    log.lock().unwrap().push(format!(
                        "{}{target}",
                        if not_modified { "304 " } else { "" }
                    ));
                    let file = match target.as_str() {
                        "/packages.json" => Some("packages.json"),
                        "/list.json"
                        | "/list.json?filter=psr%2F%2A"
                        | "/list.json?vendor=psr&filter=psr%2F%2A" => Some("list.json"),
                        "/search.json?q=log&type=" => Some("search-log.json"),
                        "/search.json?q=container&type=library" => {
                            Some("search-container-library.json")
                        }
                        "/p2/psr/log.json" => Some("p2/psr/log.json"),
                        "/p2/psr/log~dev.json" => Some("p2/psr/log~dev.json"),
                        "/p2/psr/container.json" => Some("p2/psr/container.json"),
                        _ => None,
                    };
                    let response = match file {
                        Some(_) if not_modified => {
                            "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n".to_string()
                        }
                        Some(f) => {
                            let body = std::fs::read(root.join(f)).unwrap();
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nLast-Modified: {LAST_MODIFIED}\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                String::from_utf8(body).unwrap()
                            )
                        }
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                    };
                    if stream.write_all(response.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
            });
        }
    });
    port
}

fn compare(dir: &Path, cases: &[&[&str]], composer: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    for case in cases {
        let mut theirs_args = case.to_vec();
        theirs_args.push("--no-ansi");
        let theirs = run(composer, dir, &theirs_args);
        let ours = run(&romposer(), dir, case);
        let t = (
            theirs.status.code(),
            String::from_utf8_lossy(&theirs.stdout).into_owned(),
        );
        let o = (
            ours.status.code(),
            String::from_utf8_lossy(&ours.stdout).into_owned(),
        );
        if t != o {
            failures.push(format!(
                "{case:?}\n--- composer {:?}\n{}\n--- romposer {:?}\n{}\n--- romposer stderr\n{}",
                t.0,
                t.1,
                o.0,
                o.1,
                String::from_utf8_lossy(&ours.stderr)
            ));
        }
    }
    failures
}

#[test]
fn show_and_search_match_composer_on_a_recorded_repository() {
    let Some(composer) = composer() else { return };
    let log = Arc::new(Mutex::new(Vec::new()));
    let port = serve_fixtures(log.clone());
    let dir = temp_dir("repository");
    std::fs::write(
        dir.join("composer.json"),
        format!(
            r#"{{"name":"acme/repo","require":{{"psr/log":"^3"}},"repositories":[{{"type":"composer","url":"http://127.0.0.1:{port}"}},{{"packagist.org":false}}],"config":{{"secure-http":false}}}}"#
        ),
    )
    .unwrap();
    let lock = run(&composer, &dir, &["update", "--no-install", "--no-ansi"]);
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let cases: &[&[&str]] = &[
        &["show", "-a", "psr/log", "--format=json"],
        &["show", "-a", "psr/log"],
        &["show", "-a", "psr/container", "--format=json"],
        &["show", "-a", "psr/container"],
        &["show", "-a", "psr/container", "1.1.0"],
        &["show", "-a", "psr/log", "^1", "--format=json"],
        &["show", "-a"],
        &["show", "-a", "psr/*"],
        &["show", "-a", "-N"],
        &["show", "--all"],
        &["show", "--all", "--format=json"],
        &["show", "--all", "psr/log", "--format=json"],
        &["show", "--locked"],
        &["show", "--locked", "--format=json"],
        &["show", "--locked", "psr/log", "--format=json"],
        &["show", "--locked", "psr/log"],
        &["show", "-s"],
        &["show", "-s", "--format=json"],
        &["show", "-p", "--format=json"],
        &["show", "nope/nope"],
        &["show", "-a", "nope/nope"],
        &["show", "ext-json"],
        &["search", "log"],
        &["search", "-f", "json", "log"],
        &["search", "-t", "library", "container"],
        &["search", "-N", "psr"],
        &["search", "-N", "^psr/"],
        &["search", "-O", "psr"],
        &["search", "-f", "json", "-N", "log"],
        &["search", "-N", "-O", "x"],
        &["search", "-f", "xml", "x"],
    ];
    let failures = compare(&dir, cases, &composer);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    // Composer's cache layout, and the second run revalidates with If-Modified-Since
    let cache = dir.join(format!(".composer-home/cache/repo/http---127.0.0.1-{port}"));
    for file in [
        "packages.json",
        "provider-psr~log.json",
        "provider-psr~log~dev.json",
        "package-list.txt",
        "vendor-list.txt",
    ] {
        assert!(cache.join(file).is_file(), "missing cache file {file}");
    }
    let before = log.lock().unwrap().len();
    let out = run(
        &romposer(),
        &dir,
        &["show", "-a", "psr/log", "--format=json"],
    );
    assert!(out.status.success());
    let requests: Vec<String> = log.lock().unwrap()[before..].to_vec();
    assert!(
        requests.contains(&"304 /p2/psr/log.json".to_string()),
        "no revalidation: {requests:?}"
    );
}

#[test]
fn show_symfony_console_available_matches_live_packagist() {
    let Some(composer) = composer() else { return };
    let dir = temp_dir("packagist");
    let probe = run(
        &composer,
        &dir,
        &[
            "show",
            "-a",
            "symfony/console",
            "--format=json",
            "--no-ansi",
        ],
    );
    if !probe.status.success() {
        eprintln!("skipping live packagist comparison: composer failed");
        return;
    }
    let failures = compare(
        &dir,
        &[&["show", "-a", "symfony/console", "--format=json"]],
        &composer,
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
