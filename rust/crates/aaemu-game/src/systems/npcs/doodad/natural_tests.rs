use super::*;
use crate::systems::npcs::doodad::templates::{
    DoodadFuncFinal, DoodadFuncGroups, DoodadFuncGrowth, DoodadFuncTimer, DoodadPhaseFunc,
    DoodadTemplate,
};

#[test]
fn spawn_key_is_stable_and_identifies_entry_and_position() {
    let k = spawn_key(2336, 1234.56, 789.04, 101.3);
    assert_eq!(k, "2336:12346:7890:1013");
    // The same authored position always gives the same key (same 0.1 m cell).
    assert_eq!(k, spawn_key(2336, 1234.57, 789.03, 101.31));
    assert_ne!(k, spawn_key(2337, 1234.56, 789.04, 101.3));
    assert_ne!(k, spawn_key(2336, 1234.7, 789.04, 101.3));
    assert_eq!(spawn_key(7, -1.0, 0.0, 0.04), "7:-10:0:0");
}

fn data_with(template: u32, group: u32, func: FuncTemplate) -> DoodadData {
    let mut d = DoodadData::default();
    let g = DoodadFuncGroups {
        id: group,
        almighty: template,
        group_kind_id: 1,
        ..DoodadFuncGroups::default()
    };
    d.templates.insert(
        template,
        DoodadTemplate {
            id: template,
            func_groups: vec![g],
            ..DoodadTemplate::default()
        },
    );
    let class = func.func_type();
    d.phase_funcs
        .entry(group)
        .or_default()
        .push(DoodadPhaseFunc {
            group_id: group,
            func_id: func.id(),
            func_type: class.into(),
        });
    d.phase_func_templates
        .entry(class)
        .or_default()
        .insert(func.id(), func);
    d
}

#[test]
fn only_long_timed_templates_are_tracked() {
    let timer = |delay, next_phase| {
        FuncTemplate::Timer(DoodadFuncTimer {
            id: 1,
            delay,
            next_phase,
            ..Default::default()
        })
    };
    let long = MIN_TRACKED_DELAY_MS;
    assert!(tracks_state(&data_with(1, 10, timer(long, 11)), 1));
    assert!(!tracks_state(&data_with(1, 10, timer(long - 1, 11)), 1));
    assert!(!tracks_state(&data_with(1, 10, timer(long, 0)), 1)); // no next phase
    let growth = FuncTemplate::Growth(DoodadFuncGrowth {
        id: 1,
        delay: long,
        ..Default::default()
    });
    assert!(tracks_state(&data_with(1, 10, growth), 1));
    let fin = |after| {
        FuncTemplate::Final(DoodadFuncFinal {
            id: 1,
            after,
            ..Default::default()
        })
    };
    assert!(tracks_state(&data_with(1, 10, fin(long)), 1));
    assert!(!tracks_state(&data_with(1, 10, fin(0)), 1)); // immediate removal
    assert!(!tracks_state(&DoodadData::default(), 1)); // unknown template
}

#[tokio::test]
#[ignore = "requires AAEMU_TEST_MYSQL; uses a connection-local temporary table"]
async fn rows_round_trip_through_mysql_batches() {
    let url = std::env::var("AAEMU_TEST_MYSQL").expect("private MySQL URL");
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TEMPORARY TABLE doodad_spawn_states (world_id int unsigned NOT NULL, \
         spawn_key varchar(64) NOT NULL, template_id int unsigned NOT NULL, \
         func_group_id int unsigned NOT NULL, plant_time bigint NOT NULL, \
         growth_time bigint NOT NULL DEFAULT 0, phase_time bigint NOT NULL DEFAULT 0, \
         data int NOT NULL DEFAULT 0, interaction_counts text NULL, \
         updated bigint NOT NULL DEFAULT 0, PRIMARY KEY (world_id, spawn_key))",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = |n: u32| StateRow {
        world_id: 0,
        spawn_key: spawn_key(n, 1.0, 2.0, 3.0),
        template_id: n + 1,
        func_group_id: 10,
        plant_time: 1_700_000_000,
        growth_time_ms: 5,
        phase_time_ms: 1_700_000_123_456,
        data: -3,
        interaction_counts: "{\"7\":2}".into(),
        updated: 9,
    };
    let states = NaturalStates::default();
    // More rows than one batch, a replaced row and a deleted one.
    let rows: Vec<StateRow> = (1..=(BATCH as u32 + 20)).map(row).collect();
    let refs: Vec<&StateRow> = rows.iter().collect();
    for chunk in refs.chunks(BATCH) {
        write_saves(&pool, chunk).await.unwrap();
    }
    let mut changed = row(1);
    changed.func_group_id = 11;
    write_saves(&pool, &[&changed]).await.unwrap();
    let gone = (0, spawn_key(2, 1.0, 2.0, 3.0));
    write_deletes(&pool, &[&gone]).await.unwrap();
    assert_eq!(states.load(&pool, 0).await.unwrap(), BATCH + 19);
    let restore = states.shared.restore.lock().unwrap();
    assert_eq!(restore[&(0, changed.spawn_key.clone())], changed);
    assert!(!restore.contains_key(&gone));
    assert_eq!(restore[&(0, row(5).spawn_key)], row(5));
}
