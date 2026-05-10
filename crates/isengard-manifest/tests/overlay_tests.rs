use isengard_manifest::merge_compose_yaml;
use serde_yaml::Value;

fn parse(s: &str) -> Value {
    serde_yaml::from_str(s).unwrap()
}

#[test]
fn scalar_last_write_wins() {
    let base = "services:\n  web:\n    image: nginx:1.0\n";
    let overlay = "services:\n  web:\n    image: nginx:2.0\n";
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    let img = v["services"]["web"]["image"].as_str().unwrap();
    assert_eq!(img, "nginx:2.0");
}

#[test]
fn environment_list_append_and_dedupe_by_key() {
    let base = "services:\n  web:\n    environment:\n      - A=1\n      - B=2\n";
    let overlay = "services:\n  web:\n    environment:\n      - A=9\n      - C=3\n";
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    let envs: Vec<String> = v["services"]["web"]["environment"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(envs, vec!["A=9", "B=2", "C=3"]);
}

#[test]
fn volumes_append_dedupe_by_source() {
    let base = "services:\n  web:\n    volumes:\n      - ./data:/data\n      - ./logs:/logs\n";
    let overlay =
        "services:\n  web:\n    volumes:\n      - ./data:/var/data\n      - ./conf:/etc\n";
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    let vols: Vec<String> = v["services"]["web"]["volumes"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    // ./data should replace, ./logs kept, ./conf appended.
    assert!(vols.contains(&"./data:/var/data".to_string()));
    assert!(vols.contains(&"./logs:/logs".to_string()));
    assert!(vols.contains(&"./conf:/etc".to_string()));
    assert_eq!(vols.len(), 3);
}

#[test]
fn new_service_in_overlay_inserts() {
    let base = "services:\n  web:\n    image: nginx\n";
    let overlay = "services:\n  db:\n    image: postgres\n";
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    assert_eq!(v["services"]["web"]["image"].as_str().unwrap(), "nginx");
    assert_eq!(v["services"]["db"]["image"].as_str().unwrap(), "postgres");
}

#[test]
fn nested_deploy_merges() {
    let base = r#"
services:
  web:
    deploy:
      replicas: 1
      resources:
        limits:
          cpus: "1"
"#;
    let overlay = r#"
services:
  web:
    deploy:
      replicas: 3
"#;
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    assert_eq!(
        v["services"]["web"]["deploy"]["replicas"].as_u64().unwrap(),
        3
    );
    assert_eq!(
        v["services"]["web"]["deploy"]["resources"]["limits"]["cpus"]
            .as_str()
            .unwrap(),
        "1"
    );
}

#[test]
fn ports_append_dedupe() {
    let base = "services:\n  web:\n    ports:\n      - 80:80\n      - 443:443\n";
    let overlay = "services:\n  web:\n    ports:\n      - 80:8080\n";
    let merged = merge_compose_yaml(base, &[overlay.into()]).unwrap();
    let v = parse(&merged);
    let ports: Vec<String> = v["services"]["web"]["ports"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert!(ports.contains(&"80:8080".to_string()));
    assert!(ports.contains(&"443:443".to_string()));
    assert_eq!(ports.len(), 2);
}

#[test]
fn empty_overlay_is_noop() {
    let base = "services:\n  web:\n    image: nginx\n";
    let merged = merge_compose_yaml(base, &["".into()]).unwrap();
    let v = parse(&merged);
    assert_eq!(v["services"]["web"]["image"].as_str().unwrap(), "nginx");
}

#[test]
fn three_way_merge_order_preserved() {
    let base = "services:\n  web:\n    image: a\n";
    let o1 = "services:\n  web:\n    image: b\n";
    let o2 = "services:\n  web:\n    image: c\n";
    let merged = merge_compose_yaml(base, &[o1.into(), o2.into()]).unwrap();
    let v = parse(&merged);
    assert_eq!(v["services"]["web"]["image"].as_str().unwrap(), "c");
}
