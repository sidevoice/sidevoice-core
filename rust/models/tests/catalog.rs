//! Catalogue soundness and the resolver's offers against the shared vectors.

use serde_json::{json, Value};

use crate::models::{
    catalog, catalog::CATALOG_JSON, catalog_problems, catalog_text, json::field_str, json::values,
    offers,
};

const VECTORS_JSON: &str = include_str!("../../../assets/catalog/models/vectors.json");

#[test]
fn shipped_catalogue_is_sound_and_catalogue_bytes_remain_the_source_contract() {
    assert_eq!(catalog_problems(catalog()), Vec::<String>::new());
    assert_eq!(catalog_text(), CATALOG_JSON);

    let mut broken = catalog().clone();
    broken["engines"][0]["packages"][0]["download"]["sha256"] = json!("short");
    broken["families"]["whisper"]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "mood", "kind": "colour-wheel"
        }));
    let problems = catalog_problems(&broken);
    assert!(problems
        .iter()
        .any(|problem| problem.contains("download with sha256")));
    assert!(problems
        .iter()
        .any(|problem| problem.contains("unknown kind \"colour-wheel\"")));
}

#[test]
fn shared_catalogue_offer_vectors_match_for_shipped_and_fixture_data() {
    let vectors: Value = serde_json::from_str(VECTORS_JSON).unwrap();
    let fixture = &vectors["fixture"];
    assert!(catalog_problems(fixture).is_empty());
    for vector in values(&vectors["vectors"]) {
        let name = field_str(vector, "name").unwrap_or("unnamed vector");
        let selected_catalog = match field_str(vector, "catalog") {
            Some("fixture") => fixture,
            _ => catalog(),
        };
        let place = field_str(vector, "place").unwrap_or("");
        let actual = offers(selected_catalog, &vector["capabilities"], place);
        if vector.get("error").is_some() {
            assert!(actual.is_err(), "{name}: {actual:?}");
        } else {
            let actual = actual.unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                vector["offers"],
                "{name}"
            );
        }
    }
}
