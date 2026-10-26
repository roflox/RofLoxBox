//! Typed views of the settings data blocks. Offsets are into the data block.

use super::values::*;

/// A level stored as `0x80 + level - first`.
fn level(byte: u8, first: u8) -> Option<u8> {
    byte.checked_sub(0x80).map(|n| n + first)
}

/// `MYSETTING.DAT`: player settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerSettings {
    pub on_air_display: OnOff,
    /// 1–5.
    pub lcd_brightness: Option<u8>,
    pub quantize: OnOff,
    pub auto_cue_level: AutoCueLevel,
    pub language: Language,
    pub jog_ring_brightness: Illumination,
    pub jog_ring_indicator: OnOff,
    pub slip_flashing: OnOff,
    pub disc_slot_illumination: Illumination,
    pub sync: OnOff,
    pub quantize_beat_value: QuantizeBeatValue,
    pub hot_cue_autoload: HotCueAutoload,
    pub hot_cue_color: OnOff,
    pub time_mode: TimeMode,
    pub jog_mode: JogMode,
    pub auto_cue: OnOff,
    pub master_tempo: OnOff,
    pub tempo_range: TempoRange,
    pub phase_meter: PhaseMeter,
}

impl PlayerSettings {
    pub(super) fn from_data(d: &[u8]) -> Self {
        Self {
            on_air_display: OnOff::from_byte(d[0x08]),
            lcd_brightness: level(d[0x09], 0),
            quantize: OnOff::from_byte(d[0x0a]),
            auto_cue_level: AutoCueLevel::from_byte(d[0x0b]),
            language: Language::from_byte(d[0x0c]),
            jog_ring_brightness: Illumination::from_byte(d[0x0e]),
            jog_ring_indicator: OnOff::from_byte(d[0x0f]),
            slip_flashing: OnOff::from_byte(d[0x10]),
            disc_slot_illumination: Illumination::from_byte(d[0x14]),
            sync: OnOff::from_byte(d[0x16]),
            quantize_beat_value: QuantizeBeatValue::from_byte(d[0x18]),
            hot_cue_autoload: HotCueAutoload::from_byte(d[0x19]),
            hot_cue_color: OnOff::from_byte(d[0x1a]),
            time_mode: TimeMode::from_byte(d[0x20]),
            jog_mode: JogMode::from_byte(d[0x21]),
            auto_cue: OnOff::from_byte(d[0x22]),
            master_tempo: OnOff::from_byte(d[0x23]),
            tempo_range: TempoRange::from_byte(d[0x24]),
            phase_meter: PhaseMeter::from_byte(d[0x25]),
        }
    }

    /// Writes the fields into a data block of this kind; other bytes are left as they are.
    pub(super) fn write_into(&self, d: &mut [u8]) {
        d[0x08] = self.on_air_display.to_byte();
        if let Some(level) = self.lcd_brightness {
            d[0x09] = 0x80 + level;
        }
        d[0x0a] = self.quantize.to_byte();
        d[0x0b] = self.auto_cue_level.to_byte();
        d[0x0c] = self.language.to_byte();
        d[0x0e] = self.jog_ring_brightness.to_byte();
        d[0x0f] = self.jog_ring_indicator.to_byte();
        d[0x10] = self.slip_flashing.to_byte();
        d[0x14] = self.disc_slot_illumination.to_byte();
        d[0x16] = self.sync.to_byte();
        d[0x18] = self.quantize_beat_value.to_byte();
        d[0x19] = self.hot_cue_autoload.to_byte();
        d[0x1a] = self.hot_cue_color.to_byte();
        d[0x20] = self.time_mode.to_byte();
        d[0x21] = self.jog_mode.to_byte();
        d[0x22] = self.auto_cue.to_byte();
        d[0x23] = self.master_tempo.to_byte();
        d[0x24] = self.tempo_range.to_byte();
        d[0x25] = self.phase_meter.to_byte();
    }
}

/// `MYSETTING2.DAT`: additional player settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Player2Settings {
    pub vinyl_speed_adjust: VinylSpeedAdjust,
    pub jog_display_mode: JogDisplayMode,
    /// 1–4.
    pub pad_button_brightness: Option<u8>,
    /// 1–5.
    pub jog_lcd_brightness: Option<u8>,
    pub waveform_divisions: WaveformDivisions,
    pub waveform_area: WaveformArea,
    pub beat_jump_beat_value: BeatJumpBeatValue,
}

impl Player2Settings {
    pub(super) fn from_data(d: &[u8]) -> Self {
        Self {
            vinyl_speed_adjust: VinylSpeedAdjust::from_byte(d[0x00]),
            jog_display_mode: JogDisplayMode::from_byte(d[0x01]),
            pad_button_brightness: level(d[0x02], 0),
            jog_lcd_brightness: level(d[0x03], 0),
            waveform_divisions: WaveformDivisions::from_byte(d[0x04]),
            waveform_area: WaveformArea::from_byte(d[0x0a]),
            beat_jump_beat_value: BeatJumpBeatValue::from_byte(d[0x0c]),
        }
    }

    /// Writes the fields into a data block of this kind; other bytes are left as they are.
    pub(super) fn write_into(&self, d: &mut [u8]) {
        d[0x00] = self.vinyl_speed_adjust.to_byte();
        d[0x01] = self.jog_display_mode.to_byte();
        if let Some(level) = self.pad_button_brightness {
            d[0x02] = 0x80 + level;
        }
        if let Some(level) = self.jog_lcd_brightness {
            d[0x03] = 0x80 + level;
        }
        d[0x04] = self.waveform_divisions.to_byte();
        d[0x0a] = self.waveform_area.to_byte();
        d[0x0c] = self.beat_jump_beat_value.to_byte();
    }
}

/// `DEVSETTING.DAT`: display settings for the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSettings {
    pub overview_waveform_type: OverviewWaveformType,
    pub waveform_color: WaveformColor,
    pub key_display_format: KeyDisplayFormat,
    pub waveform_current_position: WaveformCurrentPosition,
}

impl DeviceSettings {
    pub(super) fn from_data(d: &[u8]) -> Self {
        Self {
            overview_waveform_type: OverviewWaveformType::from_byte(d[0x09]),
            waveform_color: WaveformColor::from_byte(d[0x0a]),
            key_display_format: KeyDisplayFormat::from_byte(d[0x0c]),
            waveform_current_position: WaveformCurrentPosition::from_byte(d[0x0d]),
        }
    }

    /// Writes the fields into a data block of this kind; other bytes are left as they are.
    pub(super) fn write_into(&self, d: &mut [u8]) {
        d[0x09] = self.overview_waveform_type.to_byte();
        d[0x0a] = self.waveform_color.to_byte();
        d[0x0c] = self.key_display_format.to_byte();
        d[0x0d] = self.waveform_current_position.to_byte();
    }
}

/// `DJMMYSETTING.DAT`: mixer settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerSettings {
    pub channel_fader_curve: ChannelFaderCurve,
    pub crossfader_curve: CrossfaderCurve,
    pub headphones_mix: HeadphonesMix,
    pub headphones_monitoring: HeadphonesMonitoring,
    pub beat_fx_quantize: OnOff,
    pub mic_low_cut: OnOff,
    pub talk_over_mode: TalkOverMode,
    pub talk_over_level: TalkOverLevel,
    /// 1–16.
    pub midi_channel: Option<u8>,
    pub midi_button_type: MidiButtonType,
    pub display_brightness: DisplayBrightness,
    /// 1–3.
    pub indicator_brightness: Option<u8>,
    pub long_channel_fader_curve: LongChannelFaderCurve,
}

impl MixerSettings {
    pub(super) fn from_data(d: &[u8]) -> Self {
        Self {
            channel_fader_curve: ChannelFaderCurve::from_byte(d[0x0c]),
            crossfader_curve: CrossfaderCurve::from_byte(d[0x0d]),
            headphones_mix: HeadphonesMix::from_byte(d[0x0e]),
            headphones_monitoring: HeadphonesMonitoring::from_byte(d[0x0f]),
            beat_fx_quantize: OnOff::from_byte(d[0x10]),
            mic_low_cut: OnOff::from_byte(d[0x11]),
            talk_over_mode: TalkOverMode::from_byte(d[0x12]),
            talk_over_level: TalkOverLevel::from_byte(d[0x13]),
            midi_channel: level(d[0x14], 1),
            midi_button_type: MidiButtonType::from_byte(d[0x15]),
            display_brightness: DisplayBrightness::from_byte(d[0x16]),
            indicator_brightness: level(d[0x17], 1),
            long_channel_fader_curve: LongChannelFaderCurve::from_byte(d[0x18]),
        }
    }

    /// Writes the fields into a data block of this kind; other bytes are left as they are.
    pub(super) fn write_into(&self, d: &mut [u8]) {
        d[0x0c] = self.channel_fader_curve.to_byte();
        d[0x0d] = self.crossfader_curve.to_byte();
        d[0x0e] = self.headphones_mix.to_byte();
        d[0x0f] = self.headphones_monitoring.to_byte();
        d[0x10] = self.beat_fx_quantize.to_byte();
        d[0x11] = self.mic_low_cut.to_byte();
        d[0x12] = self.talk_over_mode.to_byte();
        d[0x13] = self.talk_over_level.to_byte();
        if let Some(level) = self.midi_channel {
            d[0x14] = 0x7f + level;
        }
        d[0x15] = self.midi_button_type.to_byte();
        d[0x16] = self.display_brightness.to_byte();
        if let Some(level) = self.indicator_brightness {
            d[0x17] = 0x7f + level;
        }
        d[0x18] = self.long_channel_fader_curve.to_byte();
    }
}
