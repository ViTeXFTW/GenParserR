use genparser_analysis::Analyzer;
use genparser_schema::{Schema, EMBEDDED_SCHEMA_JSON};

#[test]
fn committed_schema_round_trips_and_builds_analyzer() {
    let schema = Schema::from_json(EMBEDDED_SCHEMA_JSON).expect("committed schema parses");
    let json = schema
        .to_json_pretty()
        .expect("committed schema serializes");
    let round_tripped = Schema::from_json(&json).expect("pretty schema parses");

    let idx = round_tripped.index();
    assert!(idx.block("Object").is_some(), "Object block missing");
    assert!(idx.block("Weapon").is_some(), "Weapon block missing");
    assert!(
        idx.module("ActiveBody").is_some(),
        "ActiveBody module missing"
    );
    assert!(
        idx.value_set("weapon_slot").is_some(),
        "weapon_slot value set missing"
    );

    let analyzer = Analyzer::new(round_tripped);
    assert!(analyzer.block("Object").is_some(), "Analyzer lost Object");
    assert!(
        analyzer.module("ActiveBody").is_some(),
        "Analyzer lost ActiveBody"
    );
    assert!(
        analyzer.value_set("weapon_slot").is_some(),
        "Analyzer lost weapon_slot"
    );
}
