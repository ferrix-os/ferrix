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
use crate::policy::{Action, COOL_SECONDS, Policy, Why};

fn plan_of(amps: &[u32], seconds: u32) -> Plan {
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count: 0,
        seconds,
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
