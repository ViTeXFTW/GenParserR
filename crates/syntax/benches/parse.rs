// SPDX-License-Identifier: GPL-3.0-or-later
//! Parse-path benchmarks: raw tokenization and full CST construction over
//! synthetic INI documents sized like real game data (1k / 10k / 50k lines).
//!
//! These numbers gate the incremental-reparse decision (roadmap Phase 3): if a
//! full reparse of the largest realistic file fits the per-keystroke budget,
//! block-splicing stays out and only analysis caching goes in.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use genparser_syntax::lexer::tokenize;
use genparser_syntax::parser::FixedOpeners;
use genparser_syntax::parse;

/// Generate a synthetic document of roughly `target_lines` lines, shaped like
/// real ZH data: a mix of flat `Weapon` blocks and `Object` blocks with nested
/// module scopes and `ConditionState` sub-blocks.
fn synthetic_ini(target_lines: usize) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    let mut i = 0usize;
    while lines < target_lines {
        if i % 2 == 0 {
            out.push_str(&format!(
                "Weapon GenBenchWeapon{i}\n\
                 \x20 PrimaryDamage = 40.0\n\
                 \x20 PrimaryDamageRadius = 5.0\n\
                 \x20 SecondaryDamage = 10.0\n\
                 \x20 SecondaryDamageRadius = 10.0\n\
                 \x20 AttackRange = 150.0\n\
                 \x20 MinimumAttackRange = 10.0\n\
                 \x20 DamageType = ARMOR_PIERCING\n\
                 \x20 DeathType = EXPLODED\n\
                 \x20 WeaponSpeed = 600.0\n\
                 \x20 ProjectileObject = GenBenchProjectile{i}\n\
                 \x20 FireSound = TankFire\n\
                 \x20 ScatterRadius = 2.5\n\
                 \x20 AcceptableAimDelta = 5.0 ; degrees\n\
                 \x20 RadiusDamageAngle = 180.0\n\
                 End\n\n"
            ));
            lines += 17;
        } else {
            out.push_str(&format!(
                "Object GenBenchTank{i}\n\
                 \x20 Side = America\n\
                 \x20 BuildCost = 900\n\
                 \x20 BuildTime = 10.0\n\
                 \x20 VisionRange = 150.0\n\
                 \x20 KindOf = VEHICLE SELECTABLE\n\
                 \x20 Draw = W3DModelDraw ModuleTag_Draw\n\
                 \x20   ConditionState NONE\n\
                 \x20     Animation = GenBenchTank{i}.Idle\n\
                 \x20   End\n\
                 \x20   ConditionState REALLYDAMAGED\n\
                 \x20     Animation = GenBenchTank{i}.IdleDamaged\n\
                 \x20   End\n\
                 \x20 End\n\
                 \x20 Body = ActiveBody ModuleTag_Body\n\
                 \x20   MaxHealth = 300.0\n\
                 \x20   InitialHealth = 300.0\n\
                 \x20 End\n\
                 \x20 Behavior = ArmorUpgrade ModuleTag_Armor\n\
                 \x20   TriggeredBy = Upgrade_GenBench\n\
                 \x20 End\n\
                 End\n\n"
            ));
            lines += 23;
        }
        i += 1;
    }
    out
}

fn bench_parse(c: &mut Criterion) {
    // Nested scope keywords mirror what the schema oracle opens; file-scope
    // lines open blocks unconditionally in the parser itself.
    let oracle = FixedOpeners::new(["Draw", "Body", "Behavior", "ConditionState"]);

    let mut group = c.benchmark_group("syntax");
    group.sample_size(50);
    for &lines in &[1_000usize, 10_000, 50_000] {
        let src = synthetic_ini(lines);
        // A malformed generator would skew numbers via error-recovery paths.
        assert!(
            parse(&src, &oracle).errors.is_empty(),
            "synthetic input must parse clean"
        );
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::new("tokenize", lines), &src, |b, src| {
            b.iter(|| black_box(tokenize(src)))
        });
        group.bench_with_input(BenchmarkId::new("parse", lines), &src, |b, src| {
            b.iter(|| black_box(parse(src, &oracle)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
