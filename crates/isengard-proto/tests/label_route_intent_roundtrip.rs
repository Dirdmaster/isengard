use isengard_proto::pb::{ContainerLabelsReport, LabelRouteIntent};
use prost::Message;

#[test]
fn label_route_intents_round_trip() {
    let report = ContainerLabelsReport {
        container_id: "cid-plex".into(),
        container_name: "plex".into(),
        image: "lscr.io/linuxserver/plex:latest".into(),
        labels: [("isengard.expose".into(), "plex.vallee.casa".into())]
            .into_iter()
            .collect(),
        label_route_intents: vec![LabelRouteIntent {
            name: String::new(),
            hostname: "plex.vallee.casa".into(),
            container_port: 32400,
            unresolved_reason: String::new(),
        }],
    };

    let mut buf = Vec::new();
    report.encode(&mut buf).unwrap();
    let got = ContainerLabelsReport::decode(buf.as_slice()).unwrap();

    assert_eq!(got.label_route_intents.len(), 1);
    assert_eq!(got.label_route_intents[0].hostname, "plex.vallee.casa");
    assert_eq!(got.label_route_intents[0].container_port, 32400);
    assert!(got.label_route_intents[0].unresolved_reason.is_empty());
}
