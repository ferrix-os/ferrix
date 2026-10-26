//! The power driver's policy on fixed inputs.

extern crate std;

use std::format;

use ferrix_native_abi::types::{
    POWER_READ_CEILING, POWER_READ_FLAGS, POWER_READ_PRESENT, POWER_READ_VR, POWER_READ_WORDS,
};

use crate::decode::{self, Reading};
use crate::options::{self, DEFAULT_HOT_CELSIUS, TestMode, Words};

/// The driver's words.
const WORDS: Words<'static> = Words {
    vr: b"ferrix.power.vr",
    hot: b"ferrix.power.hot",
    test: b"ferrix.power.test",
};

/// [`options::parse`] with the driver's words.
fn parse(line: &[u8]) -> options::Options {
    options::parse(line, &WORDS)
}
use crate::plan::{self, MAX_STEPS, Plan, PlanError};
use crate::policy::{ADAPT_NEAR, Action, COOL_SECONDS, Policy, Why};

fn plan_of(amps: &[u32], seconds: u32) -> Plan {
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count: 0,
        seconds,
        adaptive: false,
    };
    for (slot, &value) in plan.amps.iter_mut().zip(amps) {
        *slot = value;
        plan.count += 1;
    }
    plan
}

#[test]
fn plan_words_give_their_plans() {
    assert_eq!(
        plan::parse(b"30,34,37,40,44@60"),
        Ok(plan_of(&[30, 34, 37, 40, 44], 60))
    );
    assert_eq!(plan::parse(b"25@1"), Ok(plan_of(&[25], 1)));
    assert_eq!(plan::parse(b"30,30@3600"), Ok(plan_of(&[30, 30], 3600)));
    assert_eq!(
        plan::parse(b"1,2,3,4,5,6,7,8@9"),
        Ok(plan_of(&[1, 2, 3, 4, 5, 6, 7, 8], 9))
    );
}

#[test]
fn malformed_plan_words_are_refused() {
    for word in [
        &b""[..],
        b"30",
        b"@60",
        b"30@",
        b"30@0",
        b"30@3601",
        b"30@00060",
        b"30,,34@60",
        b"300@60",
        b"0@60",
        b"30.5@60",
        b"+30@60",
        b"30 ,34@60",
        b"1,2,3,4,5,6,7,8,9@9",
    ] {
        assert_eq!(
            plan::parse(word),
            Err(PlanError::Malformed),
            "{:?}",
            core::str::from_utf8(word)
        );
    }
    assert_eq!(plan::parse(b"34,30@60"), Err(PlanError::Descending));
}

#[test]
fn the_plan_steps_each_period_and_keeps_its_last() {
    let plan = plan_of(&[30, 34, 37], 60);
    let steps: [usize; 6] = [0, 59, 60, 119, 120, 100_000].map(|at| plan.step_at(at));
    assert_eq!(steps, [0, 0, 1, 1, 2, 2]);
    assert_eq!(plan.step(7), 37);
}

#[test]
fn steps_above_the_ceiling_are_dropped() {
    let plan = plan_of(&[30, 34, 37, 40, 44], 60);
    assert_eq!(plan.below(40), Some((plan_of(&[30, 34, 37, 40], 60), 1)));
    assert_eq!(plan.below(48), Some((plan, 0)));
    assert_eq!(plan.below(29), None);
}

#[test]
fn the_command_line_gives_its_options() {
    let line = b"console=ttyS0 ferrix.powerctl ferrix.power.vr=30,34@60 ferrix.power.hot=80 \
                 ferrix.power.test=hang@3\n";
    let options = parse(line);
    assert_eq!(options.plan, Some(plan_of(&[30, 34], 60)));
    assert_eq!(options.hot, 80);
    assert_eq!(options.test, Some((TestMode::Hang, 3)));
    assert!(!options.hot_refused && !options.test_refused);

    let none = parse(b"ferrix.powerctl ferrix.power.vrx=30@1");
    assert_eq!(none.plan, None);
    assert_eq!(none.hot, DEFAULT_HOT_CELSIUS);
    assert_eq!(none.test, None);
}

#[test]
fn the_last_word_wins_and_bad_words_are_said() {
    let options = parse(b"ferrix.power.vr=30@1 ferrix.power.vr=31@2");
    assert_eq!(options.plan, Some(plan_of(&[31], 2)));
    let bad = parse(b"ferrix.power.vr=x ferrix.power.hot=49 ferrix.power.test=nap@3");
    assert_eq!(bad.plan, None);
    assert_eq!(bad.plan_refused, Some(PlanError::Malformed));
    assert!(bad.hot_refused && bad.test_refused);
    assert_eq!(bad.hot, DEFAULT_HOT_CELSIUS);
    assert!(parse(b"ferrix.power.test=spin@3").test_refused);
    for mode in [&b"exit@1"[..], b"hang@3600"] {
        let mut line = b"ferrix.power.test=".to_vec();
        line.extend_from_slice(mode);
        assert!(parse(&line).test.is_some());
    }
    assert!(parse(b"ferrix.power.test=hang@3601").test_refused);
    assert!(parse(b"ferrix.power.hot=100").hot_refused);
}

/// A reading of `core` and `package` degrees under `TjMax` 105.
fn reading(core: i64, package: i64) -> Reading {
    Reading {
        core: Some(core),
        package: Some(package),
        ratio: None,
        core_reasons: None,
        graphics_reasons: None,
        energy: None,
        vr_eighths: None,
        firmware_eighths: 200,
        ceiling_eighths: 256,
        latched: false,
        forgotten: false,
    }
}

#[test]
fn telemetry_decodes_and_absent_words_stay_absent() {
    let mut words = [0_u64; POWER_READ_WORDS];
    words[decode::TEMPERATURE_TARGET] = 105 << 16;
    words[decode::THERM_STATUS] = (1 << 31) | (27 << 16);
    words[decode::PACKAGE_THERM_STATUS] = 25 << 16;
    words[decode::PERF_STATUS] = 26 << 8;
    words[decode::CORE_LIMIT_REASONS] = 0x0001_0100;
    words[POWER_READ_VR] = 0x8000_0000 | 320;
    words[POWER_READ_CEILING] = 256;
    words[POWER_READ_FLAGS] = 2;
    words[POWER_READ_PRESENT] = (1 << decode::TEMPERATURE_TARGET)
        | (1 << decode::THERM_STATUS)
        | (1 << decode::PACKAGE_THERM_STATUS)
        | (1 << decode::PERF_STATUS)
        | (1 << decode::CORE_LIMIT_REASONS)
        | (1 << POWER_READ_VR);
    let decoded = decode::reading(&words);
    assert_eq!(decoded.core, Some(78));
    assert_eq!(decoded.package, Some(80));
    assert_eq!(decoded.hottest(), Some(80));
    assert_eq!(decoded.ratio, Some(26));
    assert_eq!(decoded.core_reasons, Some(0x0100));
    assert_eq!(decoded.graphics_reasons, None);
    assert_eq!(decoded.vr_eighths, Some(320));
    assert_eq!(decoded.ceiling_eighths, 256);
    assert!(decoded.forgotten && !decoded.latched);
    assert_eq!(format!("{}", decode::Amps(201)), "25.125");
    let mut names = [""; 11];
    let count = decode::reason_names(0x0100, &mut names);
    assert_eq!(&names[..count], ["amps/pl4"]);

    // The core's readout not valid: no core temperature.
    words[decode::THERM_STATUS] = 27 << 16;
    assert_eq!(decode::reading(&words).core, None);
    // Nothing read: nothing decoded, not zero.
    let nothing = decode::reading(&[0; POWER_READ_WORDS]);
    assert_eq!(nothing.package, None);
    assert_eq!(nothing.hottest(), None);
}

#[test]
fn package_power_comes_from_the_energy_counter() {
    // 1/16384 J units: 16384 counts in 1000 ms is 1 W.
    assert_eq!(decode::milliwatts((0, 14), (16_384, 14), 1000), Some(1000));
    // The counter wraps once.
    assert_eq!(
        decode::milliwatts((u32::MAX - 8191, 14), (8192, 14), 1000),
        Some(1000)
    );
    assert_eq!(decode::milliwatts((0, 14), (1, 14), 0), None);
    assert_eq!(decode::milliwatts((0, 14), (1, 13), 1000), None);
}

#[test]
fn the_policy_climbs_the_plan() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37], 60), 85);
    let cool = reading(60, 62);
    assert_eq!(
        policy.step(0, &cool),
        Action::Set {
            amps: 30,
            why: Why::Plan
        }
    );
    assert_eq!(policy.step(59, &cool), Action::Hold);
    assert_eq!(
        policy.step(60, &cool),
        Action::Set {
            amps: 34,
            why: Why::Plan
        }
    );
    assert_eq!(
        policy.step(120, &cool),
        Action::Set {
            amps: 37,
            why: Why::Plan
        }
    );
    assert_eq!(policy.step(10_000, &cool), Action::Hold);
}

#[test]
fn the_policy_backs_off_at_the_soft_limit_and_lifts_after_a_cool_spell() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37], 1), 85);
    let cool = reading(60, 62);
    let _ = policy.step(0, &cool);
    let _ = policy.step(1, &cool);
    let _ = policy.step(2, &cool);
    assert_eq!(policy.current(), Some(2));
    // Hot on the package: one step down.
    assert_eq!(
        policy.step(3, &reading(70, 85)),
        Action::Set {
            amps: 34,
            why: Why::BackOff { celsius: 85 }
        }
    );
    // Still hot within the spacing: hold.
    assert_eq!(policy.step(4, &reading(70, 86)), Action::Hold);
    // Warm but not cool (81 > 80): the cap holds, nothing changes.
    assert_eq!(policy.step(10, &reading(81, 81)), Action::Hold);
    // Cool from 11: lifted only after COOL_SECONDS of it.
    assert_eq!(policy.step(11, &reading(80, 79)), Action::Hold);
    assert_eq!(
        policy.step(11 + COOL_SECONDS - 1, &reading(70, 70)),
        Action::Hold
    );
    assert_eq!(
        policy.step(11 + COOL_SECONDS, &reading(70, 70)),
        Action::Set {
            amps: 37,
            why: Why::Plan
        }
    );
}

#[test]
fn the_policy_gives_up_on_the_third_back_off_or_at_the_lowest_step() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37, 40], 1), 85);
    let cool = reading(60, 62);
    for at in 0..4 {
        let _ = policy.step(at, &cool);
    }
    assert_eq!(policy.current(), Some(3));
    let hot = reading(88, 88);
    assert!(matches!(
        policy.step(10, &hot),
        Action::Set { amps: 37, .. }
    ));
    assert!(matches!(
        policy.step(15, &hot),
        Action::Set { amps: 34, .. }
    ));
    assert_eq!(policy.step(20, &hot), Action::GiveUp { celsius: 88 });
    assert!(policy.done());
    assert_eq!(policy.step(1000, &cool), Action::Hold);

    let mut lowest = Policy::new(plan_of(&[30, 34], 60), 85);
    let _ = lowest.step(0, &cool);
    assert_eq!(
        lowest.step(1, &reading(90, 70)),
        Action::GiveUp { celsius: 90 }
    );
}

#[test]
fn a_hot_start_writes_nothing_and_a_refusal_is_asked_again() {
    let mut policy = Policy::new(plan_of(&[30, 34], 1), 85);
    assert_eq!(policy.step(0, &reading(86, 70)), Action::Hold);
    assert_eq!(policy.current(), None);
    assert!(matches!(
        policy.step(1, &reading(60, 60)),
        Action::Set { amps: 34, .. }
    ));
    policy.refused(None);
    assert!(matches!(
        policy.step(2, &reading(60, 60)),
        Action::Set { amps: 34, .. }
    ));
}

#[test]
fn no_reading_neither_backs_off_nor_lifts() {
    let mut policy = Policy::new(plan_of(&[30], 60), 85);
    let blind = Reading {
        core: None,
        package: None,
        ..reading(0, 0)
    };
    assert!(matches!(
        policy.step(0, &blind),
        Action::Set { amps: 30, .. }
    ));
    assert_eq!(policy.step(1, &blind), Action::Hold);
}

#[test]
fn a_regained_claim_climbs_the_plan_again_from_its_first_step() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37], 60), 85);
    let cool = reading(60, 62);
    let _ = policy.step(0, &cool);
    let _ = policy.step(60, &cool);
    let _ = policy.step(120, &cool);
    assert_eq!(policy.current(), Some(2));
    // The lease lapsed and the claim was regained at 400 s: firmware's value
    // is in force, and the plan starts over rather than jumping to 37 A.
    policy.restart(400);
    assert_eq!(policy.current(), None);
    assert_eq!(
        policy.step(400, &cool),
        Action::Set {
            amps: 30,
            why: Why::Plan
        }
    );
    assert_eq!(policy.step(459, &cool), Action::Hold);
    assert_eq!(
        policy.step(460, &cool),
        Action::Set {
            amps: 34,
            why: Why::Plan
        }
    );
}

#[test]
fn a_regained_claim_keeps_the_back_offs_cap_and_a_give_up() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37], 1), 85);
    let cool = reading(60, 62);
    for second in 0..3 {
        let _ = policy.step(second, &cool);
    }
    assert_eq!(
        policy.step(3, &reading(86, 80)),
        Action::Set {
            amps: 34,
            why: Why::BackOff { celsius: 86 }
        }
    );
    policy.restart(10);
    assert_eq!(
        policy.step(10, &cool),
        Action::Set {
            amps: 30,
            why: Why::Plan
        }
    );
    let _ = policy.step(11, &cool);
    // Capped at 34 A: the back-off's cap outlives the lapse.
    assert_eq!(policy.step(12, &cool), Action::Hold);
    assert_eq!(policy.current(), Some(1));

    let mut given_up = Policy::new(plan_of(&[30], 1), 85);
    let _ = given_up.step(0, &cool);
    assert_eq!(
        given_up.step(1, &reading(90, 80)),
        Action::GiveUp { celsius: 90 }
    );
    given_up.restart(5);
    assert!(given_up.done());
    assert_eq!(given_up.step(5, &cool), Action::Hold);
}

#[test]
fn a_cool_spell_lifts_a_back_off_one_step_at_a_time() {
    let mut policy = Policy::new(plan_of(&[30, 34, 37, 40, 44], 1), 85);
    let cool = reading(60, 62);
    for at in 0..5 {
        let _ = policy.step(at, &cool);
    }
    assert_eq!(policy.current(), Some(4));
    // Two back-offs, 5 s apart: 44 A to 40 A to 37 A.
    let _ = policy.step(10, &reading(86, 86));
    let _ = policy.step(15, &reading(87, 87));
    assert_eq!(policy.current(), Some(2));
    // A cool spell: 40 A, not 44 A.
    let _ = policy.step(16, &reading(78, 78));
    assert_eq!(
        policy.step(16 + COOL_SECONDS, &reading(78, 78)),
        Action::Set {
            amps: 40,
            why: Why::Plan
        }
    );
    // Another: 44 A, the plan's top.
    assert_eq!(
        policy.step(17 + COOL_SECONDS, &reading(78, 78)),
        Action::Hold
    );
    assert_eq!(
        policy.step(17 + 2 * COOL_SECONDS, &reading(78, 78)),
        Action::Set {
            amps: 44,
            why: Why::Plan
        }
    );
    assert_eq!(
        policy.step(17 + 3 * COOL_SECONDS, &reading(78, 78)),
        Action::Hold
    );
}

#[test]
fn an_auto_plan_spreads_its_range_over_the_steps() {
    let plan = plan::parse(b"auto:30-44").expect("a range");
    assert!(plan.adaptive);
    assert_eq!(&plan.amps[..plan.count], &[30, 32, 34, 36, 38, 40, 42, 44]);
    assert_eq!(plan::parse(b"auto"), plan::parse(b"auto:30-44"));
    let narrow = plan::parse(b"auto:30-33").expect("a narrow range");
    assert_eq!(&narrow.amps[..narrow.count], &[30, 31, 32, 33]);
    assert_eq!(plan::parse(b"auto:44-30"), Err(PlanError::Descending));
    assert_eq!(plan::parse(b"auto:0-30"), Err(PlanError::Malformed));
    assert_eq!(plan::parse(b"auto30-44"), Err(PlanError::Malformed));
}

/// A package that settles at `30 + 1.4 C a ampere`, a tenth of the way
/// there each second: 44 A is 91.6 C, past the kernel's backstop, and 34 A
/// 77.6 C, as the N150 ran Endfield on 2026-10-25.
fn settle(celsius: f64, amps: u32) -> f64 {
    let settled = 30.0 + 1.4 * f64::from(amps);
    celsius + (settled - celsius) / 10.0
}

#[test]
fn an_auto_plan_finds_the_step_the_cooling_holds_and_never_reaches_the_limit() {
    let plan = plan::parse(b"auto:30-44").expect("a range");
    let mut policy = Policy::new(plan, 85);
    let mut celsius = 60.0;
    let mut amps = 25;
    let mut hottest = 0.0f64;
    for second in 0..3600 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a model's degrees, read as the sensor reads them"
        )]
        let now = celsius as i64;
        if let Action::Set { amps: set, .. } = policy.step(second, &reading(now, now)) {
            amps = set;
        }
        celsius = settle(celsius, amps);
        if second > 60 {
            hottest = hottest.max(celsius);
        }
    }
    assert!(hottest < 85.0, "reached {hottest} C");
    // It climbs past firmware's 25 A to a step the cooling holds, and not
    // to one it does not.
    assert!((32..=38).contains(&amps), "settled at {amps} A");
}

#[test]
fn an_auto_plan_steps_down_on_the_trend_before_the_limit() {
    let plan = plan::parse(b"auto:30-44").expect("a range");
    let mut policy = Policy::new(plan, 85);
    let _ = policy.step(0, &reading(60, 60));
    for at in 1..200 {
        let _ = policy.step(at, &reading(60, 60));
    }
    let high = policy.current().expect("a step");
    assert!(high > 0);
    // Rising 1 C a second from 60 C: the trend steps down before the
    // reading itself is within ADAPT_NEAR of the limit.
    let mut first = None;
    for (at, celsius) in (200..).zip(60..85) {
        if let Action::Set {
            why: Why::BackOff { celsius },
            ..
        } = policy.step(at, &reading(celsius, celsius))
        {
            first = Some(celsius);
            break;
        }
    }
    let first = first.expect("a back-off");
    assert!(first < 85 - ADAPT_NEAR, "first back-off at {first} C");
    assert_eq!(policy.current(), Some(high - 1));
}

#[test]
fn an_auto_plan_does_not_back_off_on_a_fast_warm_up_far_from_the_limit() {
    let plan = plan::parse(b"auto:30-44").expect("a range");
    let mut policy = Policy::new(plan, 85);
    for at in 0..200 {
        let _ = policy.step(at, &reading(50, 50));
    }
    let high = policy.current().expect("a step");
    // A game loading: 3 C a second from 50 C to 71 C, ten seconds ahead
    // past the limit, the reading far under it.
    for (at, celsius) in (200..).zip((50..=71).step_by(3)) {
        assert_eq!(policy.step(at, &reading(celsius, celsius)), Action::Hold);
    }
    assert_eq!(policy.current(), Some(high));
}
