#![allow(clippy::redundant_closure_call)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::match_single_binding)]
#![allow(clippy::clone_on_copy)]

#[doc = "`CoreContractSlice`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
pub struct CoreContractSlice {
    #[serde(rename = "languagesResponse")]
    pub languages_response: LanguagesResponse,
    #[serde(rename = "trialRefusal")]
    pub trial_refusal: TrialRefusal,
    #[serde(rename = "trialRequest")]
    pub trial_request: TrialRequest,
    #[serde(rename = "trialResponse")]
    pub trial_response: TrialResponse,
    #[serde(rename = "voiceMediaCommand")]
    pub voice_media_command: VoiceMediaCommand,
}
#[doc = "`LanguagesResponse`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct LanguagesResponse {
    pub audio_grace_seconds: f64,
    pub merge_window_secs: f64,
    pub replay_on_return_seconds: i64,
    pub smart_turn_max_silence: f64,
    pub smart_turn_min_silence: f64,
    pub stt: LanguagesStage,
    pub tts: LanguagesStage,
    pub turn_end_mode: ::std::string::String,
    pub turn_patience: ::std::string::String,
    pub ui_language: ::std::string::String,
    pub user_speech_timeout: f64,
    pub vad_confidence: f64,
    pub vad_min_volume: f64,
    pub vad_start_secs: f64,
}
#[doc = "`LanguagesStage`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct LanguagesStage {
    pub build: (),
    pub model: ::std::string::String,
    pub options: ::serde_json::Map<::std::string::String, ::serde_json::Value>,
    pub place: ::std::string::String,
}
#[doc = "`RefusalDetail`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RefusalDetail {
    pub key: ::std::string::String,
    pub message: ::std::string::String,
}
#[doc = "`TrialAudio`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TrialAudio {
    pub data_base64: ::std::string::String,
    pub encoding: TrialAudioEncoding,
    pub sample_rate: TrialAudioSampleRate,
}
#[doc = "`TrialAudioEncoding`"]
#[derive(
    :: serde :: Deserialize,
    :: serde :: Serialize,
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
)]
pub enum TrialAudioEncoding {
    #[serde(rename = "pcm_s16le")]
    PcmS16le,
}
impl ::std::fmt::Display for TrialAudioEncoding {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match *self {
            Self::PcmS16le => f.write_str("pcm_s16le"),
        }
    }
}
impl ::std::str::FromStr for TrialAudioEncoding {
    type Err = self::error::ConversionError;
    fn from_str(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        match value {
            "pcm_s16le" => Ok(Self::PcmS16le),
            _ => Err("invalid value".into()),
        }
    }
}
impl ::std::convert::TryFrom<&str> for TrialAudioEncoding {
    type Error = self::error::ConversionError;
    fn try_from(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
impl ::std::convert::TryFrom<::std::string::String> for TrialAudioEncoding {
    type Error = self::error::ConversionError;
    fn try_from(
        value: ::std::string::String,
    ) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
#[doc = "`TrialAudioSampleRate`"]
#[derive(:: serde :: Serialize, Clone, Debug)]
#[serde(transparent)]
pub struct TrialAudioSampleRate(i64);
impl ::std::ops::Deref for TrialAudioSampleRate {
    type Target = i64;
    fn deref(&self) -> &i64 {
        &self.0
    }
}
impl ::std::convert::From<TrialAudioSampleRate> for i64 {
    fn from(value: TrialAudioSampleRate) -> Self {
        value.0
    }
}
impl ::std::convert::TryFrom<i64> for TrialAudioSampleRate {
    type Error = self::error::ConversionError;
    fn try_from(value: i64) -> ::std::result::Result<Self, self::error::ConversionError> {
        if ![16000_i64].contains(&value) {
            Err("invalid value".into())
        } else {
            Ok(Self(value))
        }
    }
}
impl<'de> ::serde::Deserialize<'de> for TrialAudioSampleRate {
    fn deserialize<D>(deserializer: D) -> ::std::result::Result<Self, D::Error>
    where
        D: ::serde::Deserializer<'de>,
    {
        Self::try_from(<i64>::deserialize(deserializer)?)
            .map_err(|e| <D::Error as ::serde::de::Error>::custom(e.to_string()))
    }
}
#[doc = "`TrialOptions`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct TrialOptions {
    #[serde(skip_serializing_if = "::std::option::Option::is_none")]
    pub context: ::std::option::Option<::std::string::String>,
    #[serde(skip_serializing_if = "::std::option::Option::is_none")]
    pub language: ::std::option::Option<::std::string::String>,
}
#[doc = "`TrialRefusal`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TrialRefusal {
    pub detail: RefusalDetail,
}
#[doc = "`TrialRequest`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TrialRequest {
    pub audio: TrialAudio,
    pub model: ::std::string::String,
    #[serde(skip_serializing_if = "::std::option::Option::is_none")]
    pub options: ::std::option::Option<TrialOptions>,
    pub place: TrialRequestPlace,
}
#[doc = "`TrialRequestPlace`"]
#[derive(
    :: serde :: Deserialize,
    :: serde :: Serialize,
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
)]
pub enum TrialRequestPlace {
    #[serde(rename = "openai")]
    Openai,
}
impl ::std::fmt::Display for TrialRequestPlace {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match *self {
            Self::Openai => f.write_str("openai"),
        }
    }
}
impl ::std::str::FromStr for TrialRequestPlace {
    type Err = self::error::ConversionError;
    fn from_str(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        match value {
            "openai" => Ok(Self::Openai),
            _ => Err("invalid value".into()),
        }
    }
}
impl ::std::convert::TryFrom<&str> for TrialRequestPlace {
    type Error = self::error::ConversionError;
    fn try_from(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
impl ::std::convert::TryFrom<::std::string::String> for TrialRequestPlace {
    type Error = self::error::ConversionError;
    fn try_from(
        value: ::std::string::String,
    ) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
#[doc = "`TrialResponse`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TrialResponse {
    pub text: ::std::string::String,
}
#[doc = "`VoiceMediaCommand`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct VoiceMediaCommand {
    pub data: VoiceMediaCommandData,
    #[serde(rename = "type")]
    pub type_: VoiceMediaCommandType,
}
#[doc = "`VoiceMediaCommandData`"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct VoiceMediaCommandData {
    pub path: VoiceMediaCommandDataPath,
    pub session_id: ::std::string::String,
}
#[doc = "`VoiceMediaCommandDataPath`"]
#[derive(
    :: serde :: Deserialize,
    :: serde :: Serialize,
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
)]
pub enum VoiceMediaCommandDataPath {
    #[serde(rename = "socket")]
    Socket,
    #[serde(rename = "webrtc")]
    Webrtc,
}
impl ::std::fmt::Display for VoiceMediaCommandDataPath {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match *self {
            Self::Socket => f.write_str("socket"),
            Self::Webrtc => f.write_str("webrtc"),
        }
    }
}
impl ::std::str::FromStr for VoiceMediaCommandDataPath {
    type Err = self::error::ConversionError;
    fn from_str(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        match value {
            "socket" => Ok(Self::Socket),
            "webrtc" => Ok(Self::Webrtc),
            _ => Err("invalid value".into()),
        }
    }
}
impl ::std::convert::TryFrom<&str> for VoiceMediaCommandDataPath {
    type Error = self::error::ConversionError;
    fn try_from(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
impl ::std::convert::TryFrom<::std::string::String> for VoiceMediaCommandDataPath {
    type Error = self::error::ConversionError;
    fn try_from(
        value: ::std::string::String,
    ) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
#[doc = "`VoiceMediaCommandType`"]
#[derive(
    :: serde :: Deserialize,
    :: serde :: Serialize,
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
)]
pub enum VoiceMediaCommandType {
    #[serde(rename = "voice-media")]
    VoiceMedia,
}
impl ::std::fmt::Display for VoiceMediaCommandType {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match *self {
            Self::VoiceMedia => f.write_str("voice-media"),
        }
    }
}
impl ::std::str::FromStr for VoiceMediaCommandType {
    type Err = self::error::ConversionError;
    fn from_str(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        match value {
            "voice-media" => Ok(Self::VoiceMedia),
            _ => Err("invalid value".into()),
        }
    }
}
impl ::std::convert::TryFrom<&str> for VoiceMediaCommandType {
    type Error = self::error::ConversionError;
    fn try_from(value: &str) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
impl ::std::convert::TryFrom<::std::string::String> for VoiceMediaCommandType {
    type Error = self::error::ConversionError;
    fn try_from(
        value: ::std::string::String,
    ) -> ::std::result::Result<Self, self::error::ConversionError> {
        value.parse()
    }
}
#[doc = " Error types."]
pub mod error {
    #[doc = r" Error from a `TryFrom` or `FromStr` implementation."]
    pub struct ConversionError(::std::borrow::Cow<'static, str>);
    impl ::std::error::Error for ConversionError {}
    impl ::std::fmt::Display for ConversionError {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> Result<(), ::std::fmt::Error> {
            ::std::fmt::Display::fmt(&self.0, f)
        }
    }
    impl ::std::fmt::Debug for ConversionError {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> Result<(), ::std::fmt::Error> {
            ::std::fmt::Debug::fmt(&self.0, f)
        }
    }
    impl From<&'static str> for ConversionError {
        fn from(value: &'static str) -> Self {
            Self(value.into())
        }
    }
    impl From<String> for ConversionError {
        fn from(value: String) -> Self {
            Self(value.into())
        }
    }
}
