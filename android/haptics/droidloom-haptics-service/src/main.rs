//! Android vibrator manager AIDL V4 service for Droidloom.
//!
//! Patterns go to the host user's device broker, which plays them through
//! hapticd. The manager lists one vibrator when the host has one and none
//! otherwise, so Android sees no vibrator on a host without hapticd. Effect
//! and composition callbacks fire when the pattern's time has passed.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use android_hardware_vibrator::aidl::android::hardware::vibrator::{
    Braking::Braking,
    CompositeEffect::CompositeEffect,
    CompositePrimitive::CompositePrimitive,
    CompositePwleV2::CompositePwleV2,
    Effect::Effect,
    EffectStrength::EffectStrength,
    FrequencyAccelerationMapEntry::FrequencyAccelerationMapEntry,
    HapticGeneratorConfig::HapticGeneratorConfig,
    HapticGeneratorSession::HapticGeneratorSession,
    IVibrationSession::IVibrationSession,
    IVibrator::{self, BnVibrator, IVibrator as _},
    IVibratorCallback::IVibratorCallback,
    IVibratorManager::{BnVibratorManager, IVibratorManager},
    PrimitivePwle::PrimitivePwle,
    VendorEffect::VendorEffect,
    VibrationSessionConfig::VibrationSessionConfig,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};
use droidloom_denial_ipc::SeqPacket;
use droidloom_device_protocol::{MAX_PATTERN_MS, MAX_RECORD, Message, Step};
use droidloom_haptics::{
    COMPOSITION_DELAY_MAX_MS, COMPOSITION_SIZE_MAX, EFFECTS, PRIMITIVES, Refused, compose,
    effect_steps, primitive_ms, total_ms,
};

const SERVICE_NAME: &str = "android.hardware.vibrator.IVibratorManager/default";
/// The supervisor binds the broker's haptics socket here.
const SOCKET: &str = "/dev/socket/droidloom/haptics";
const VIBRATOR_ID: i32 = 0;

fn unsupported<T>() -> binder::Result<T> {
    Err(Status::new_exception(ExceptionCode::UNSUPPORTED_OPERATION, None))
}

fn illegal<T>() -> binder::Result<T> {
    Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None))
}

/// The connection to the broker, made again when the broker restarts.
struct Link(Mutex<Option<SeqPacket>>);

impl Link {
    fn connect() -> Option<SeqPacket> {
        let socket = SeqPacket::connect(SOCKET).ok()?;
        socket.set_io_timeout(Duration::from_secs(5)).ok()?;
        Some(socket)
    }

    fn send(&self, message: &Message) {
        let record = message.encode();
        let mut socket = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..2 {
            if socket.is_none() {
                *socket = Self::connect();
            }
            match socket.as_ref() {
                Some(connected) if connected.send_record(&record, &[]).is_ok() => return,
                Some(_) => *socket = None,
                None => return,
            }
        }
    }

    /// Whether the host has a vibrator, and whether it takes strengths.
    fn query(&self) -> (bool, bool) {
        let mut socket = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if socket.is_none() {
            *socket = Self::connect();
        }
        let Some(connected) = socket.as_ref() else {
            return (false, false);
        };
        let mut bytes = vec![0; MAX_RECORD];
        let reply = connected
            .send_record(&Message::HapticsQuery.encode(), &[])
            .ok()
            .and_then(|()| connected.receive_record_into(&mut bytes).ok())
            .and_then(|record| Message::decode(record.bytes).ok());
        match reply {
            Some(Message::HapticsDevice { present, intensity }) => (present, intensity),
            _ => {
                *socket = None;
                (false, false)
            }
        }
    }
}

struct On {
    amplitude: f32,
    /// When the vibration `on` started ends, while it plays.
    until: Option<Instant>,
}

struct Vibrator {
    link: Arc<Link>,
    intensity: bool,
    on: Mutex<On>,
}

impl Interface for Vibrator {}

/// Tells Android a pattern of `ms` has played, once it has.
fn complete_after(ms: u32, callback: Option<&Strong<dyn IVibratorCallback>>) {
    if let Some(callback) = callback.cloned() {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(u64::from(ms)));
            let _ = callback.onComplete();
        });
    }
}

fn duration_ms(ms: u32) -> binder::Result<i32> {
    i32::try_from(ms).or_else(|_| illegal())
}

impl Vibrator {
    fn play(&self, steps: Vec<Step>) {
        self.link.send(&Message::HapticsPlay(steps));
    }
}

impl IVibrator::IVibrator for Vibrator {
    fn getCapabilities(&self) -> binder::Result<i32> {
        let mut capabilities = IVibrator::CAP_ON_CALLBACK
            | IVibrator::CAP_PERFORM_CALLBACK
            | IVibrator::CAP_COMPOSE_EFFECTS;
        if self.intensity {
            capabilities |= IVibrator::CAP_AMPLITUDE_CONTROL;
        }
        Ok(capabilities)
    }

    fn off(&self) -> binder::Result<()> {
        self.on.lock().unwrap_or_else(PoisonError::into_inner).until = None;
        self.link.send(&Message::HapticsStop);
        Ok(())
    }

    fn on(
        &self,
        timeout_ms: i32,
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        let Some(ms) = u32::try_from(timeout_ms)
            .ok()
            .filter(|ms| (1..=MAX_PATTERN_MS).contains(ms))
        else {
            return illegal();
        };
        let mut on = self.on.lock().unwrap_or_else(PoisonError::into_inner);
        on.until = Some(Instant::now() + Duration::from_millis(u64::from(ms)));
        self.play(vec![Step {
            ms,
            intensity: on.amplitude,
        }]);
        drop(on);
        complete_after(ms, callback);
        Ok(())
    }

    fn perform(
        &self,
        effect: Effect,
        strength: EffectStrength,
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<i32> {
        let Some(steps) = effect_steps(effect.0, strength.0) else {
            return unsupported();
        };
        let ms = total_ms(&steps);
        self.on.lock().unwrap_or_else(PoisonError::into_inner).until = None;
        self.play(steps);
        complete_after(ms, callback);
        duration_ms(ms)
    }

    fn getSupportedEffects(&self) -> binder::Result<Vec<Effect>> {
        Ok(EFFECTS.iter().map(|effect| Effect(*effect)).collect())
    }

    fn setAmplitude(&self, amplitude: f32) -> binder::Result<()> {
        if !self.intensity {
            return unsupported();
        }
        if !(amplitude > 0.0 && amplitude <= 1.0) {
            return illegal();
        }
        let mut on = self.on.lock().unwrap_or_else(PoisonError::into_inner);
        on.amplitude = amplitude;
        // A vibration playing goes on at the new strength for what is left.
        if let Some(until) = on.until {
            let left = until.saturating_duration_since(Instant::now()).as_millis();
            match u32::try_from(left) {
                Ok(ms) if ms > 0 => self.play(vec![Step {
                    ms,
                    intensity: amplitude,
                }]),
                _ => on.until = None,
            }
        }
        Ok(())
    }

    fn setExternalControl(&self, _enabled: bool) -> binder::Result<()> {
        unsupported()
    }

    fn getCompositionDelayMax(&self) -> binder::Result<i32> {
        Ok(COMPOSITION_DELAY_MAX_MS)
    }

    fn getCompositionSizeMax(&self) -> binder::Result<i32> {
        i32::try_from(COMPOSITION_SIZE_MAX).or_else(|_| illegal())
    }

    fn getSupportedPrimitives(&self) -> binder::Result<Vec<CompositePrimitive>> {
        Ok(PRIMITIVES
            .iter()
            .map(|primitive| CompositePrimitive(*primitive))
            .collect())
    }

    fn getPrimitiveDuration(&self, primitive: CompositePrimitive) -> binder::Result<i32> {
        match primitive_ms(primitive.0) {
            Some(ms) => duration_ms(ms),
            None => unsupported(),
        }
    }

    fn compose(
        &self,
        composite: &[CompositeEffect],
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        let parts: Vec<(i32, i32, f32)> = composite
            .iter()
            .map(|part| (part.delayMs, part.primitive.0, part.scale))
            .collect();
        let steps = match compose(&parts) {
            Ok(steps) => steps,
            Err(Refused::Unsupported) => return unsupported(),
            Err(Refused::OutOfRange) => return illegal(),
        };
        let ms = total_ms(&steps);
        self.on.lock().unwrap_or_else(PoisonError::into_inner).until = None;
        self.play(steps);
        complete_after(ms, callback);
        Ok(())
    }

    fn getSupportedAlwaysOnEffects(&self) -> binder::Result<Vec<Effect>> {
        Ok(Vec::new())
    }

    fn alwaysOnEnable(
        &self,
        _id: i32,
        _effect: Effect,
        _strength: EffectStrength,
    ) -> binder::Result<()> {
        unsupported()
    }

    fn alwaysOnDisable(&self, _id: i32) -> binder::Result<()> {
        unsupported()
    }

    fn getResonantFrequency(&self) -> binder::Result<f32> {
        unsupported()
    }

    fn getQFactor(&self) -> binder::Result<f32> {
        unsupported()
    }

    fn getFrequencyResolution(&self) -> binder::Result<f32> {
        unsupported()
    }

    fn getFrequencyMinimum(&self) -> binder::Result<f32> {
        unsupported()
    }

    fn getBandwidthAmplitudeMap(&self) -> binder::Result<Vec<f32>> {
        unsupported()
    }

    fn getPwlePrimitiveDurationMax(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn getPwleCompositionSizeMax(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn getSupportedBraking(&self) -> binder::Result<Vec<Braking>> {
        unsupported()
    }

    fn composePwle(
        &self,
        _composite: &[PrimitivePwle],
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        unsupported()
    }

    fn performVendorEffect(
        &self,
        _vendor_effect: &VendorEffect,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        unsupported()
    }

    fn getFrequencyToOutputAccelerationMap(
        &self,
    ) -> binder::Result<Vec<FrequencyAccelerationMapEntry>> {
        unsupported()
    }

    fn getPwleV2PrimitiveDurationMaxMillis(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn getPwleV2CompositionSizeMax(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn getPwleV2PrimitiveDurationMinMillis(&self) -> binder::Result<i32> {
        unsupported()
    }

    fn composePwleV2(
        &self,
        _composite: &CompositePwleV2,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        unsupported()
    }
}

struct Manager {
    vibrator: Option<Strong<dyn IVibrator::IVibrator>>,
}

impl Interface for Manager {}

impl IVibratorManager for Manager {
    fn getCapabilities(&self) -> binder::Result<i32> {
        Ok(0)
    }

    fn getVibratorIds(&self) -> binder::Result<Vec<i32>> {
        Ok(self.vibrator.iter().map(|_| VIBRATOR_ID).collect())
    }

    fn getVibrator(&self, vibrator_id: i32) -> binder::Result<Strong<dyn IVibrator::IVibrator>> {
        match &self.vibrator {
            Some(vibrator) if vibrator_id == VIBRATOR_ID => Ok(vibrator.clone()),
            _ => illegal(),
        }
    }

    fn prepareSynced(&self, _vibrator_ids: &[i32]) -> binder::Result<()> {
        unsupported()
    }

    fn triggerSynced(
        &self,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<()> {
        unsupported()
    }

    fn cancelSynced(&self) -> binder::Result<()> {
        unsupported()
    }

    fn startSession(
        &self,
        _vibrator_ids: &[i32],
        _config: &VibrationSessionConfig,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<Strong<dyn IVibrationSession>> {
        unsupported()
    }

    fn clearSessions(&self) -> binder::Result<()> {
        unsupported()
    }

    fn startHapticGeneratorSession(
        &self,
        _vibrator_ids: &[i32],
        _config: &HapticGeneratorConfig,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> binder::Result<HapticGeneratorSession> {
        unsupported()
    }
}

fn main() {
    let link = Arc::new(Link(Mutex::new(None)));
    let (present, intensity) = link.query();
    eprintln!(
        "droidloom haptics: {}",
        match (present, intensity) {
            (false, _) => "the host has no vibrator",
            (true, false) => "host vibrator, on and off",
            (true, true) => "host vibrator with strengths",
        }
    );
    let vibrator = present.then(|| {
        BnVibrator::new_binder(
            Vibrator {
                link,
                intensity,
                on: Mutex::new(On {
                    amplitude: 1.0,
                    until: None,
                }),
            },
            BinderFeatures::default(),
        )
    });
    let manager = BnVibratorManager::new_binder(Manager { vibrator }, BinderFeatures::default());
    ProcessState::start_thread_pool();
    binder::add_service(SERVICE_NAME, manager.as_binder())
        .expect("register the vibrator manager");
    ProcessState::join_thread_pool();
}
