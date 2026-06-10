// SPDX-License-Identifier: GPL-3.0-or-later
//! Analysis-path benchmarks over synthetic schema-conformant documents:
//! parse with the real schema oracle, then each per-keystroke analysis pass
//! (diagnose, semantic tokens, definition extraction) in isolation.
//!
//! Together with the syntax benches these decide roadmap Phase 3: whether the
//! keystroke path needs incremental reparse, incremental analysis, or neither.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use genparser_analysis::{diagnostics, index, semantic, Analyzer};

/// Same shape as the syntax-crate generator, but kept schema-conformant so the
/// diagnostics pass runs its real (non-error) field/value validation paths.
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

fn bench_analyze(c: &mut Criterion) {
    let analyzer = Analyzer::embedded();

    let mut group = c.benchmark_group("analysis");
    group.sample_size(50);
    for &lines in &[1_000usize, 10_000, 50_000] {
        let src = synthetic_ini(lines);
        let parse = analyzer.parse(&src);
        assert!(parse.errors.is_empty(), "synthetic input must parse clean");
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", lines), &src, |b, src| {
            b.iter(|| black_box(analyzer.parse(src)))
        });
        group.bench_with_input(BenchmarkId::new("diagnose", lines), &parse, |b, parse| {
            b.iter(|| black_box(diagnostics::diagnose(&analyzer, parse, None)))
        });
        group.bench_with_input(
            BenchmarkId::new("semantic_tokens", lines),
            &parse,
            |b, parse| b.iter(|| black_box(semantic::semantic_tokens(&analyzer, parse))),
        );
        group.bench_with_input(
            BenchmarkId::new("definitions_in", lines),
            &parse,
            |b, parse| b.iter(|| black_box(index::definitions_in(&analyzer, parse, "bench.ini"))),
        );
        // The full keystroke path as the server runs it today: parse + diagnose.
        group.bench_with_input(BenchmarkId::new("keystroke", lines), &src, |b, src| {
            b.iter(|| {
                let parse = analyzer.parse(src);
                black_box(diagnostics::diagnose(&analyzer, &parse, None))
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_analyze);
criterion_main!(benches);
