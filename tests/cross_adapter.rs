use oncall_ai::alert::{Alert, AlertSource, AlertStatus};
use oncall_ai::grafana::Grafana;
use oncall_ai::pagerduty::Pagerduty;

fn sole_alert(source: &dyn AlertSource, body: &[u8]) -> Alert {
    source
        .parse(body)
        .expect("fixture parses")
        .into_iter()
        .next()
        .expect("fixture yields one alert")
}

#[test]
fn firing_checkout_alerts_agree_on_the_shared_contract() {
    let grafana = sole_alert(
        &Grafana,
        include_bytes!("../fixtures/grafana/firing_single.json"),
    );
    let pagerduty = sole_alert(
        &Pagerduty,
        include_bytes!("../fixtures/pagerduty/triggered.json"),
    );

    for alert in [&grafana, &pagerduty] {
        assert_eq!(alert.status, AlertStatus::Firing);
        assert_eq!(
            alert.labels.get("service").map(String::as_str),
            Some("checkout"),
            "service must live under the same label key for {}",
            alert.source
        );
        assert!(!alert.source_alert_id.is_empty());
    }
    assert_ne!(grafana.source, pagerduty.source);
}
