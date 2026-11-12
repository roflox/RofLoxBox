#![allow(clippy::unwrap_used)] // tests: helpers outside #[test] functions too
use std::path::{Path, PathBuf};

use deckhand_formats::settings::values::*;
use deckhand_formats::settings::{Settings, SettingsFile, SettingsKind};

fn settings_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().unwrap().to_str().unwrap().ends_with("SETTING.DAT")
                || path.file_name().unwrap().to_str().unwrap().ends_with("SETTING2.DAT")
            {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"), &mut files);
    files
}

#[test]
fn every_settings_fixture_parses_with_valid_checksum() {
    let files = settings_files();
    assert!(files.len() > 70, "found only {} settings fixtures", files.len());
    for path in files {
        let file = SettingsFile::open(&path).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        let (kind, brand) = match name {
            "DEVSETTING.DAT" => (SettingsKind::Device, "PIONEER DJ"),
            "DJMMYSETTING.DAT" => (SettingsKind::Mixer, "PioneerDJ"),
            "MYSETTING2.DAT" => (SettingsKind::Player2, "PIONEER"),
            _ => (SettingsKind::Player, "PIONEER"),
        };
        assert_eq!(file.kind, kind, "{}", path.display());
        assert_eq!(file.brand, brand, "{}", path.display());
        assert_eq!(file.software, "rekordbox");
        assert!(file.checksum_is_valid(), "{}", path.display());
    }
}

#[test]
fn corrupted_data_fails_checksum() {
    let path = settings_files().into_iter().find(|p| p.ends_with("MYSETTING.DAT")).unwrap();
    let mut bytes = std::fs::read(path).unwrap();
    bytes[0x68 + 3] ^= 0x01;
    assert!(!SettingsFile::from_bytes(&bytes).unwrap().checksum_is_valid());
}

#[test]
fn truncated_input_errors() {
    let path = settings_files().into_iter().next().unwrap();
    let bytes = std::fs::read(path).unwrap();
    for len in 0..bytes.len() - 3 {
        assert!(SettingsFile::from_bytes(&bytes[..len]).is_err(), "len {len}");
    }
}

fn fixture(path: &str) -> Settings {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path);
    SettingsFile::open(path).unwrap().settings().unwrap()
}

/// For each fixture that changes one option, decoding must differ from the
/// defaults in exactly that field. A wrong offset shows up as a second change.
#[test]
fn each_option_fixture_changes_one_field() {
    macro_rules! check {
        ($variant:ident, $group:literal, $file:literal, [$($dir:literal => |$s:ident| $change:expr),+ $(,)?]) => {
            let Settings::$variant(defaults) = fixture(concat!("complete_export/empty/PIONEER/", $file)) else {
                panic!("defaults are not {}", stringify!($variant));
            };
            $(
                let Settings::$variant(actual) = fixture(concat!("settings/", $group, "/", $dir, "/", $file)) else {
                    panic!("{} is not {}", $dir, stringify!($variant));
                };
                let mut expected = defaults;
                let $s = &mut expected;
                $change;
                assert_eq!(actual, expected, "{}", $dir);
            )+
        };
    }

    check!(Device, "devsetting", "DEVSETTING.DAT", [
        "keydisplayformat-alphanumeric" => |s| s.key_display_format = KeyDisplayFormat::Alphanumeric,
        "overviewwaveformtype-full" => |s| s.overview_waveform_type = OverviewWaveformType::Full,
        "waveformcolor-3band" => |s| s.waveform_color = WaveformColor::ThreeBand,
        "waveformcolor-rgb" => |s| s.waveform_color = WaveformColor::Rgb,
        "waveformcurrentposition-left" => |s| s.waveform_current_position = WaveformCurrentPosition::Left,
    ]);

    check!(Mixer, "djmmysetting", "DJMMYSETTING.DAT", [
        "beatfxquantize_off" => |s| s.beat_fx_quantize = OnOff::Off,
        "chfadercurve_steepbottom" => |s| s.channel_fader_curve = ChannelFaderCurve::SteepBottom,
        "chfadercurve_steeptop" => |s| s.channel_fader_curve = ChannelFaderCurve::SteepTop,
        "chfadercurvelong_exp2" => |s| s.long_channel_fader_curve = LongChannelFaderCurve::Exponential2,
        "chfadercurvelong_linear" => |s| s.long_channel_fader_curve = LongChannelFaderCurve::Linear,
        "crossfadercurve_constantpower" => |s| s.crossfader_curve = CrossfaderCurve::ConstantPower,
        "crossfadercurve_slowcut" => |s| s.crossfader_curve = CrossfaderCurve::SlowCut,
        "headphones_monosplit_monosplit" => |s| s.headphones_monitoring = HeadphonesMonitoring::MonoSplit,
        "headphones_preeq_preeq" => |s| s.headphones_mix = HeadphonesMix::PreEq,
        "miclowcut_off" => |s| s.mic_low_cut = OnOff::Off,
        "midi_buttontype_trigger" => |s| s.midi_button_type = MidiButtonType::Trigger,
        "midi_ch_15" => |s| s.midi_channel = Some(15),
        "midi_ch_16" => |s| s.midi_channel = Some(16),
        "midi_ch_2" => |s| s.midi_channel = Some(2),
        "mixer_brightness_display_1" => |s| s.display_brightness = DisplayBrightness::Level(1),
        "mixer_brightness_display_2" => |s| s.display_brightness = DisplayBrightness::Level(2),
        "mixer_brightness_display_white" => |s| s.display_brightness = DisplayBrightness::White,
        "mixer_brightness_indicator_1" => |s| s.indicator_brightness = Some(1),
        "mixer_brightness_indicator_2" => |s| s.indicator_brightness = Some(2),
        "talkover_level_12" => |s| s.talk_over_level = TalkOverLevel::Minus12Db,
        "talkover_level_24" => |s| s.talk_over_level = TalkOverLevel::Minus24Db,
        "talkover_level_6" => |s| s.talk_over_level = TalkOverLevel::Minus6Db,
        "talkover_mode_normal" => |s| s.talk_over_mode = TalkOverMode::Normal,
    ]);

    check!(Player, "mysetting", "MYSETTING.DAT", [
        "autocue_off" => |s| s.auto_cue = OnOff::Off,
        "autocuelevel_36" => |s| s.auto_cue_level = AutoCueLevel::Minus36Db,
        "autocuelevel_42" => |s| s.auto_cue_level = AutoCueLevel::Minus42Db,
        "autocuelevel_72" => |s| s.auto_cue_level = AutoCueLevel::Minus72Db,
        "autocuelevel_78" => |s| s.auto_cue_level = AutoCueLevel::Minus78Db,
        "discslotillumination_dark" => |s| s.disc_slot_illumination = Illumination::Dark,
        "discslotillumination_off" => |s| s.disc_slot_illumination = Illumination::Off,
        "hotcueautoload_off" => |s| s.hot_cue_autoload = HotCueAutoload::Off,
        "hotcueautoload_rekordboxsetting" => |s| s.hot_cue_autoload = HotCueAutoload::RekordboxSetting,
        "hotcuecolor_on" => |s| s.hot_cue_color = OnOff::On,
        "jogmode_cdj" => |s| s.jog_mode = JogMode::Cdj,
        "jogringbrightness_dark" => |s| s.jog_ring_brightness = Illumination::Dark,
        "jogringbrightness_off" => |s| s.jog_ring_brightness = Illumination::Off,
        "jogringindicator_off" => |s| s.jog_ring_indicator = OnOff::Off,
        "language_french" => |s| s.language = Language::French,
        "language_greek" => |s| s.language = Language::Greek,
        "language_turkish" => |s| s.language = Language::Turkish,
        "lcdbrightness_1" => |s| s.lcd_brightness = Some(1),
        "lcdbrightness_5" => |s| s.lcd_brightness = Some(5),
        "mastertempo_on" => |s| s.master_tempo = OnOff::On,
        "onairdisplay_off" => |s| s.on_air_display = OnOff::Off,
        "phasemeter_type2" => |s| s.phase_meter = PhaseMeter::Type2,
        "quantize_eighth" => |s| s.quantize_beat_value = QuantizeBeatValue::Eighth,
        "quantize_half" => |s| s.quantize_beat_value = QuantizeBeatValue::Half,
        "quantize_off" => |s| s.quantize = OnOff::Off,
        "quantize_quarter" => |s| s.quantize_beat_value = QuantizeBeatValue::Quarter,
        "slipflashing_off" => |s| s.slip_flashing = OnOff::Off,
        "sync_on" => |s| s.sync = OnOff::On,
        "temporange_16" => |s| s.tempo_range = TempoRange::Sixteen,
        "temporange_6" => |s| s.tempo_range = TempoRange::Six,
        "temporange_wide" => |s| s.tempo_range = TempoRange::Wide,
        "timemode_elapsed" => |s| s.time_mode = TimeMode::Elapsed,
    ]);

    check!(Player2, "mysetting2", "MYSETTING2.DAT", [
        "beatjumpbeatvalue_64" => |s| s.beat_jump_beat_value = BeatJumpBeatValue::SixtyFour,
        "beatjumpbeatvalue_half" => |s| s.beat_jump_beat_value = BeatJumpBeatValue::Half,
        "beatjumpbeatvalue_one" => |s| s.beat_jump_beat_value = BeatJumpBeatValue::One,
        "jogdisplaymode_artwork" => |s| s.jog_display_mode = JogDisplayMode::Artwork,
        "jogdisplaymode_info" => |s| s.jog_display_mode = JogDisplayMode::Info,
        "jogdisplaymode_simple" => |s| s.jog_display_mode = JogDisplayMode::Simple,
        "joglcdbrightness_1" => |s| s.jog_lcd_brightness = Some(1),
        "joglcdbrightness_5" => |s| s.jog_lcd_brightness = Some(5),
        "padbuttonbrightness_1" => |s| s.pad_button_brightness = Some(1),
        "padbuttonbrightness_4" => |s| s.pad_button_brightness = Some(4),
        "vinylspeedadjust_release" => |s| s.vinyl_speed_adjust = VinylSpeedAdjust::Release,
        "vinylspeedadjust_touchrelease" => |s| s.vinyl_speed_adjust = VinylSpeedAdjust::TouchAndRelease,
        "waveform_phasemeter" => |s| s.waveform_area = WaveformArea::PhaseMeter,
        "waveformdivisions_timescale" => |s| s.waveform_divisions = WaveformDivisions::TimeScale,
    ]);
}

/// No field of the default files decodes to an unknown value.
#[test]
fn defaults_decode_fully() {
    for file in ["DEVSETTING.DAT", "DJMMYSETTING.DAT", "MYSETTING.DAT", "MYSETTING2.DAT"] {
        let decoded = format!("{:?}", fixture(&format!("complete_export/empty/PIONEER/{file}")));
        assert!(!decoded.contains("Unknown") && !decoded.contains("None"), "{file}: {decoded}");
    }
}

#[test]
fn option_bytes_round_trip() {
    for byte in 0..=u8::MAX {
        assert_eq!(TempoRange::from_byte(byte).to_byte(), byte);
        assert_eq!(AutoCueLevel::from_byte(byte).to_byte(), byte);
        assert_eq!(DisplayBrightness::from_byte(byte).to_byte(), byte);
    }
}
