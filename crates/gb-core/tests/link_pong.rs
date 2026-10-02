//! Link Pong (homebrew/link-pong) on two Game Boys joined by a link cable.
//!
//! Needs the ROM, built with RGBDS: `node scripts/build-homebrew.js`. Without
//! it these tests say so and pass (CI builds it first).

use gb_core::{Button, FrameEnd, GameBoy};
use std::collections::BTreeMap;

const ROM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/games/link-pong.gb");

// The game's variables: the first WRAM bytes, in link-pong.asm's order.
const W_ROLE: u16 = 0xC000;
const W_STATE: u16 = 0xC001;
const W_BALL_X: u16 = 0xC002;
const W_P1_Y: u16 = 0xC006;
const W_P2_Y: u16 = 0xC007;
const W_SCORE1: u16 = 0xC008;
const W_SCORE2: u16 = 0xC009;
const W_FRAME: u16 = 0xC00F;
const W_TITLE_NOTE: u16 = 0xC012;
/// wState through wServeWait: everything the lockstep keeps the same.
const GAME_STATE: std::ops::Range<u16> = 0xC001..0xC00B;

const ROLE_MASTER: u8 = 1;
const ROLE_SLAVE: u8 = 2;
const ROLE_CPU: u8 = 3;
const ST_PLAY: u8 = 1;
const ST_OVER: u8 = 2;

fn load() -> Option<GameBoy> {
    match std::fs::read(ROM) {
        Ok(rom) => Some(GameBoy::new(rom).unwrap()),
        Err(_) => {
            eprintln!("skipped: {ROM} not built (node scripts/build-homebrew.js)");
            None
        }
    }
}

/// The host between two linked Game Boys: one's master byte to the other.
fn carry(from: &mut GameBoy, to: &mut GameBoy) {
    while let Some(byte) = from.take_link_out() {
        let back = to.link_clocked(byte);
        from.link_answer(back);
    }
}

/// One frame on each, carrying bytes whenever one has to wait.
fn frame_linked(a: &mut GameBoy, b: &mut GameBoy) {
    for flip in [false, true] {
        let (x, y) = if flip {
            (&mut *b, &mut *a)
        } else {
            (&mut *a, &mut *b)
        };
        while x.run_frame().unwrap() == FrameEnd::LinkWait {
            carry(x, y);
        }
        carry(x, y);
        x.take_audio();
    }
}

/// Holds `button` for a few frames, then lets go.
fn press_linked(a: &mut GameBoy, b: &mut GameBoy, on_a: bool, button: Button) {
    let gb = |a: &mut GameBoy, b: &mut GameBoy, down| {
        if on_a {
            a.set_button(button, down)
        } else {
            b.set_button(button, down)
        }
    };
    gb(a, b, true);
    for _ in 0..5 {
        frame_linked(a, b);
    }
    gb(a, b, false);
    for _ in 0..5 {
        frame_linked(a, b);
    }
}

fn state(gb: &GameBoy) -> Vec<u8> {
    GAME_STATE.map(|a| gb.peek(a)).collect()
}

/// Two linked Game Boys at the title screen, then A presses START.
fn linked_game() -> Option<(GameBoy, GameBoy)> {
    let (mut a, mut b) = (load()?, load()?);
    a.plug_link(true);
    b.plug_link(true);
    for _ in 0..30 {
        frame_linked(&mut a, &mut b); // both reach the title screen
    }
    press_linked(&mut a, &mut b, true, Button::Start);
    Some((a, b))
}

#[test]
fn pressing_start_makes_that_side_player_1_and_starts_both() {
    let Some((a, b)) = linked_game() else { return };
    assert_eq!(a.peek(W_ROLE), ROLE_MASTER);
    assert_eq!(b.peek(W_ROLE), ROLE_SLAVE);
    assert_eq!((a.peek(W_STATE), b.peek(W_STATE)), (ST_PLAY, ST_PLAY));
}

#[test]
fn both_game_boys_play_exactly_the_same_game() {
    let Some((mut a, mut b)) = linked_game() else {
        return;
    };
    // Player 1 holds up, player 2 holds down: each paddle should move on
    // both screens, and the ball goes past them until someone wins.
    a.set_button(Button::Up, true);
    b.set_button(Button::Down, true);
    let (mut seen_a, mut seen_b) = (BTreeMap::new(), BTreeMap::new());
    for _ in 0..4000 {
        frame_linked(&mut a, &mut b);
        seen_a.insert(a.peek(W_FRAME), state(&a));
        seen_b.insert(b.peek(W_FRAME), state(&b));
        if a.peek(W_STATE) == ST_OVER && b.peek(W_STATE) == ST_OVER {
            break;
        }
    }
    // At every step both reached, the game is the same, byte for byte.
    let mut compared = 0;
    for (step, sa) in &seen_a {
        if let Some(sb) = seen_b.get(step) {
            assert_eq!(sa, sb, "step {step}");
            compared += 1;
        }
    }
    assert!(compared > 100, "compared {compared} steps");
    for gb in [&a, &b] {
        assert_eq!(gb.peek(W_STATE), ST_OVER, "someone won");
        assert_eq!(gb.peek(W_P1_Y), 16, "player 1's paddle went to the top");
        assert_eq!(gb.peek(W_P2_Y), 144 - 24, "player 2's to the bottom");
    }
    let scores = |gb: &GameBoy| (gb.peek(W_SCORE1), gb.peek(W_SCORE2));
    assert_eq!(scores(&a), scores(&b));
    assert!(scores(&a).0 == 9 || scores(&a).1 == 9, "{:?}", scores(&a));
}

#[test]
fn pulling_the_cable_mid_game_takes_both_back_to_the_title() {
    let Some((mut a, mut b)) = linked_game() else {
        return;
    };
    for _ in 0..60 {
        frame_linked(&mut a, &mut b);
    }
    a.plug_link(false);
    b.plug_link(false);
    for _ in 0..400 {
        a.run_frame().unwrap();
        b.run_frame().unwrap();
    }
    // "LINK LOST" on the title screen's row 14, from column 5 (tile numbers:
    // L = 19, I = 17, N = 21, K = 18).
    let note =
        |gb: &GameBoy| -> Vec<u8> { (0..4).map(|i| gb.peek(0x9800 + 14 * 32 + 5 + i)).collect() };
    for gb in [&a, &b] {
        assert_eq!(gb.peek(W_ROLE), 0, "back on the title screen");
        assert_eq!(note(gb), [19, 17, 21, 18], "saying LINK LOST");
        assert_eq!(gb.peek(W_TITLE_NOTE), 0, "(said once)");
    }
}

#[test]
fn alone_select_plays_the_computer() {
    let Some(mut gb) = load() else { return };
    for _ in 0..30 {
        gb.run_frame().unwrap();
    }
    gb.set_button(Button::Select, true);
    for _ in 0..5 {
        gb.run_frame().unwrap();
    }
    gb.set_button(Button::Select, false);
    assert_eq!(gb.peek(W_ROLE), ROLE_CPU);
    let x0 = gb.peek(W_BALL_X);
    for _ in 0..120 {
        gb.run_frame().unwrap();
    }
    assert_eq!(gb.peek(W_STATE), ST_PLAY);
    assert_ne!(gb.peek(W_BALL_X), x0, "the ball is moving");
}
