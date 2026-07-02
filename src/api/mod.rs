//! Vendored onboarding subset of OpenBubbles rust/src/api/api.rs @ a7fab47, de-FRB'd.
//! rustpush re-exports below were provided by the dropped mirrors include.
// This module deliberately mirrors upstream's api.rs re-export surface. As a
// binary crate we have no external consumer, so the `pub use` re-exports read
// as unused imports here — silence them file-wide rather than surgically
// trimming the mirror (which would make re-vendor painful).
#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(clippy::result_large_err)] // PushError is a 176-byte upstream type; we mirror the signature rather than change it.

pub mod buffered_conn;
use buffered_conn::BufferedApsConn;
pub mod runtime;
use crate::api::runtime::init_logger;



use std::{borrow::{Borrow, BorrowMut}, collections::HashSet, fs::{self, File}, future::Future, io::{Cursor, Read, Write}, ops::Deref, panic, path::Path, str::FromStr, sync::{Arc, OnceLock, Weak}, time::Duration};
pub use std::time::SystemTime;
use anyhow::anyhow;
#[cfg(not(target_os = "android"))]
use keystore::software::{SoftwareEncryptor, SoftwareKeystore};
use keystore::{AesKeystoreKey, EcCurve, EcKeystoreKey, EncryptMode, KeystoreAccessRules, KeystoreDigest, KeystoreEncryptKey, KeystorePadding, RsaKey, init_keystore, keystore};
pub use rustpush::{default_provider, ArcAnisetteClient, LoginClientInfo, DefaultAnisetteProvider};
use log::{debug, error, info, warn};
use plist::{Data, Dictionary};
pub use plist::Value;
use sha2::Digest;

pub use rustpush::DebugMutex as Mutex;
pub use std::path::PathBuf;
use prost::Message as prostMessage;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::{runtime::Runtime, select, sync::{broadcast, mpsc, watch, RwLock}};
pub use mpsc::Sender;
pub use rustpush::{APSMessage, CircleClientSession, CircleServerSession, EntitlementAuthState, IDSNGMIdentity, LoginDelegate, MADRID_SERVICE, TokenProvider, authenticate_apple, authenticate_phone, authenticate_smsless, cloud_messages::CloudMessagesClient, cloudkit::{CloudKitClient, CloudKitState}, facetime::{FACETIME_SERVICE, FTClient, FTState, VIDEO_SERVICE}, findmy::{FindMyClient, FindMyState, FindMyStateManager, MULTIPLEX_SERVICE}, keychain::{KeychainClient, KeychainClientState}, login_apple_delegates, name_photo_sharing::ProfilesClient, sharedstreams::{AssetMetadata, FFMpegFilePackager, FileMetadata, FilePackager, PreparedAsset, PreparedFile, SharedStreamClient, SharedStreamsState, SyncController, SyncManager, SyncState}, statuskit::{ChannelInterestToken, StatusKitClient, StatusKitState, StatusKitStatus}};
use rustpush::{AnisetteProvider, DebugRwLock, cloudkit::contact_info_to_handle, cloudkit_proto::{CuttlefishSerializedKey, base64_encode}, findmy::SharedBeaconClient, keychain::{CloudKey, CurrentBottle, SivKey}, passwords::PasswordState, request_update_account};
pub use rustpush::findmy::{FindMyFriendsClient, FindMyPhoneClient};
pub use rustpush::sharedstreams::{SharedAlbum, SyncStatus};
pub use rustpush::cloudkit_proto::EscrowData;
pub use rustpush::passwords::PasswordManager;
use rand::Rng;
use uuid::Uuid;
use rustpush::KeyCache;
use std::io::Seek;
use async_recursion::async_recursion;
use base64::prelude::*;
pub use rustpush::IdmsAuthListener;
pub use broadcast::Receiver;









// --- rustpush re-exports (from mirrors.rs) ---
pub use rustpush::name_photo_sharing::{IMessageNameRecord, IMessagePosterRecord, IMessageNicknameRecord};
pub use rustpush::{DeleteTarget, MoveToRecycleBinMessage, OperatedChat};
pub use rustpush::{SetTranscriptBackgroundMessage, ShareProfileMessage, SharedPoster, UpdateProfileSharingMessage, UpdateProfileMessage, NSArrayClass, TextFlags, TextEffect, TextFormat, ScheduleMode, SupportAction, NSArray, SupportAlert, PrivateDeviceInfo, PermanentDeleteMessage, NormalMessage, MessageType, UpdateExtensionMessage, ErrorMessage, UnsendMessage, EditMessage, PartExtension, IconChangeMessage, RichLinkImageAttachmentSubstitute, ChangeParticipantMessage, ReactMessage, Reaction, ReactMessageType, RenameMessage, LPLinkMetadata, LPSpecializationMetadata, NSURL, LPIconMetadata, LPImageMetadata, LinkMeta, ExtensionApp, NSDictionaryClass, BalloonLayout, Balloon, IndexedMessagePart, AttachmentType, macos::MacOSConfig, Message, MessageTarget, macos::HardwareConfig, APSConnection, APSConnectionResource, APSState, Attachment, AuthPhone, IDSUserIdentity, MMCSFile, MessageInst, MessagePart, MessageParts, OSConfig, RelayConfig, ResourceState};
pub use rustpush::{TypingApp, ApsData, ApsAlert, AkData, IdmsCircleMessage, IdmsRequestedSignIn, TeardownSignIn, IdmsMessage, CertifiedContext, PushError, IDSUser, IMClient, ConversationData, ReportMessage, register};
pub use rustpush::{LoginState, AppleAccount, VerifyBody, TrustedPhoneNumber};
pub use rustpush::findmy::{Follow, Address, Location, FoundDevice};
pub use rustpush::facetime::{FTSession, FTMode, FTParticipant, FTMember, LetMeInRequest, FTMessage};
pub use rustpush::facetime::facetimep::{ConversationParticipant, ConversationLink};
pub use rustpush::statuskit::{StatusKitPersonalConfig, StatusKitMessage};
pub use rustpush::posterkit::{TranscriptDynamicUserData, WatchBackground, SimplifiedTranscriptPoster, SimplifiedIncomingCallPoster, PosterRole, UIColor, PRPosterContentMaterialStyle, PosterAsset, PRPosterSystemTimeFontConfiguration, PRPosterColor, PRPosterTitleStyleConfiguration, WallpaperMetadata, PosterColor, PhotoPosterContentsFrame, PhotoPosterContentsSize, PhotoPosterLayer, PhotoPosterLayout, PhotoPosterProperties, PhotoPosterContents, MonogramData, MemojiData, PosterType, SimplifiedPoster};
pub use rustpush::findmy::BeaconAttributes;
pub use rustpush::UpdateAccountFinish;
pub use rustpush::cloud_messages::{CloudChat, CloudMessageSummary, CloudProp, CloudParticipant, GZipWrapper, MMCSAttachmentMeta, AttachmentMeta, NumOrString, AttachmentMetaExtra, CloudProp001, CloudMessage, CloudAttachment, MessageFlags, cloudmessagesp::{ChatProto, MessageProto, MessageProto2, MessageProto3, MessageProto4}};
pub use rustpush::cloudkit_proto::Asset;
pub use plist::Date;
pub use rustpush::{NSAttributedString, NSDictionaryTypedCoder, StCollapsedValue, NSNumber, NSString};
pub use rustpush::passwords::{PasswordManagerMeta, ShareInviteContentData, PasswordRawEntry, WifiPassword, PasswordManagerMetaDataFormerlyShared, PasswordManagerMetaData, PasswordManagerMetaChange, PasswordManagerAltDomain, PasswordManagerTotp, PasswordManagerMetaDataCtx, Passkey};
pub use rustpush::cloud_messages::{MessageEdit, MessageEditRange, MessageSummaryInfo};
pub use rustpush::findmy::{BeaconNamingRecord, LocationReport};

pub fn do_first_time_init(path: String) {
    let dir = PathBuf::from_str(&path).unwrap();

    init_logger(&dir);

    // Register the process-wide keystore before any rustpush identity/push op
    // touches `keystore()`. Upstream does this inside SharedPushState::restore;
    // we don't vendor that struct, so do it here. `init_keystore` is a
    // OnceLock::set, so calling it once at boot is sufficient and idempotent.
    let keystore_path = dir.join("keystore.plist");
    #[cfg(not(target_os = "android"))]
    init_keystore(SoftwareKeystore {
        state: plist::from_file(&keystore_path).unwrap_or_default(),
        update_state: Box::new(move |state| {
            match plist_to_buf(state) {
                Ok(buf) => {
                    if let Err(e) = crate::persist::write_atomic(&keystore_path, &buf) {
                        error!("failed to persist keystore.plist: {e}");
                    }
                }
                Err(e) => error!("failed to serialize keystore.plist: {e}"),
            }
        }),
        encryptor: SoftwareEncryptor(*b"desktopisinsecureyoushouldn'tber"),
    });
}

fn plist_to_string<T: serde::Serialize>(value: &T) -> Result<String, plist::Error> {
    plist_to_buf(value).map(|val| String::from_utf8(val).unwrap())
}

fn plist_to_buf<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, plist::Error> {
    let mut buf: Vec<u8> = Vec::new();
    let writer = Cursor::new(&mut buf);
    plist::to_writer_xml(writer, &value)?;
    Ok(buf)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

pub fn decode_identity(identity: &[u8]) -> anyhow::Result<IDSNGMIdentity> {
    Ok(IDSNGMIdentity::restore(identity, "openbubbles")?)
}

/// Read the persisted hardware/push/identity/config blob (`hw_info.plist`),
/// written by [`setup_push`]. `None` if absent or unparseable.
pub fn read_hardware(path: String) -> Option<SavedHardwareState> {
    let dir = PathBuf::from_str(&path).unwrap();
    plist::from_file::<_, SavedHardwareState>(dir.join("hw_info.plist")).ok()
}

/// Read the persisted registered users (`id.plist`), written by [`register_ids`].
/// `None` if absent or unparseable.
pub fn restore_users(path: String) -> Option<Vec<IDSUser>> {
    let dir = PathBuf::from_str(&path).unwrap();
    plist::from_file::<_, Vec<IDSUser>>(dir.join("id.plist")).ok()
}

pub fn bin_serialize<S>(x: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(x)
}

fn bin_deserialize_16<'de, D>(d: D) -> Result<[u8; 16], D::Error>
where
    D: Deserializer<'de>,
{
    let s: Data = Deserialize::deserialize(d)?;
    let s: Vec<u8> = s.into();
    Ok(s.try_into().unwrap())
}

pub fn bin_deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Data = Deserialize::deserialize(d)?;
    Ok(s.into())
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum JoinedOSConfig {
    MacOS(Arc<MacOSConfig>),
    Relay(Arc<RelayConfig>),
}

impl JoinedOSConfig {
    fn config(&self) -> Arc<dyn OSConfig> {
        match self {
            Self::MacOS(conf) => conf.clone(),
            Self::Relay(conf) => conf.clone(),
        }
    }
}

impl Deref for JoinedOSConfig {
    type Target = dyn OSConfig;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::MacOS(conf) => conf.as_ref(),
            Self::Relay(conf) => conf.as_ref(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedHardwareState {
    pub push: APSState,
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub identity: Vec<u8>,
    pub os_config: JoinedOSConfig,
}

pub struct HwExtra {
    pub version: String,
    pub protocol_version: u32,
    pub device_id: String,
    pub icloud_ua: String,
    pub aoskit_version: String,
}

pub struct DeviceInfo {
    pub name: String,
    pub serial: String,
    pub os_version: String,
    pub encoded_data: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
pub struct AnisetteState {
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize_16")]
    keychain_identifier: [u8; 16],
    provisioned: Option<ProvisionedAnisette>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProvisionedAnisette {
    client_secret: Data,
    mid: Data,
    metadata: Data,
    rinfo: String,
    #[serde(default)]
    flavor: ProvisionedFlavor,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub enum ProvisionedFlavor {
    #[default]
    Mac,
    Ios,
}

pub fn get_device_info(config: &JoinedOSConfig) -> anyhow::Result<DeviceInfo> {
    let debug_info = config.get_debug_meta();
    Ok(DeviceInfo {
        name: debug_info.hardware_version.clone(),
        serial: debug_info.serial_number.clone(),
        os_version: debug_info.user_version.clone(),
        encoded_data: match config {
            JoinedOSConfig::MacOS(config) => {
                let copied = config.as_ref().clone();
                Some(crate::bbhwinfo::HwInfo {
                    inner: Some(crate::bbhwinfo::hw_info::InnerHwInfo {
                        product_name: copied.inner.product_name,
                        io_mac_address: copied.inner.io_mac_address.to_vec(),
                        platform_serial_number: copied.inner.platform_serial_number,
                        platform_uuid: copied.inner.platform_uuid,
                        root_disk_uuid: copied.inner.root_disk_uuid,
                        board_id: copied.inner.board_id,
                        os_build_num: copied.inner.os_build_num,
                        platform_serial_number_enc: copied.inner.platform_serial_number_enc,
                        platform_uuid_enc: copied.inner.platform_uuid_enc,
                        root_disk_uuid_enc: copied.inner.root_disk_uuid_enc,
                        rom: copied.inner.rom,
                        rom_enc: copied.inner.rom_enc,
                        mlb: copied.inner.mlb,
                        mlb_enc: copied.inner.mlb_enc
                    }),
                    version: copied.version,
                    protocol_version: copied.protocol_version as i32,
                    device_id: copied.device_id,
                    icloud_ua: copied.icloud_ua,
                    aoskit_version: copied.aoskit_version,
                }.encode_to_vec())
            },
            JoinedOSConfig::Relay(_) => None
        }
    })
}

pub fn new_ngm_identity() -> anyhow::Result<IDSNGMIdentity> {
    Ok(IDSNGMIdentity::new()?)
}

pub fn duplicate_user(user: &IDSUser) -> IDSUser {
    user.clone()
}

pub fn generate_udid() -> String {
    let udid: [u8; 32] = rand::thread_rng().gen();
    encode_hex(&udid).to_uppercase()
}

pub fn config_from_validation_data(data: Vec<u8>, extra: HwExtra) -> anyhow::Result<JoinedOSConfig> {
    let inner = HardwareConfig::from_validation_data(&data)?;
    Ok(JoinedOSConfig::MacOS(Arc::new(MacOSConfig {
        inner,
        version: extra.version,
        protocol_version: extra.protocol_version,
        device_id: extra.device_id,
        icloud_ua: extra.icloud_ua,
        aoskit_version: extra.aoskit_version,
        udid: Some(generate_udid()),
    })))
}

pub async fn config_from_relay(code: String, host: String, token: &Option<String>) -> anyhow::Result<JoinedOSConfig> {
    Ok(JoinedOSConfig::Relay(Arc::new(RelayConfig {
        version: RelayConfig::get_versions(&host, &code, token).await?,
        icloud_ua: "com.apple.iCloudHelper/282 CFNetwork/1408.0.4 Darwin/22.5.0".to_string(),
        aoskit_version: "com.apple.AOSKit/282 (com.apple.accountsd/113)".to_string(),
        dev_uuid: Uuid::new_v4().to_string(),
        protocol_version: 1660,
        host: host.clone(),
        code: code.clone(),
        beeper_token: token.clone(),
        udid: Some(generate_udid()),
    })))
}

pub fn config_from_encoded(encoded: Vec<u8>) -> anyhow::Result<JoinedOSConfig> {
    let copied = crate::bbhwinfo::HwInfo::decode(&mut Cursor::new(encoded))?;
    let inner = copied.inner.unwrap();
    Ok(JoinedOSConfig::MacOS(Arc::new(MacOSConfig {
        inner: HardwareConfig {
            product_name: inner.product_name,
            io_mac_address: inner.io_mac_address.try_into().unwrap(),
            platform_serial_number: inner.platform_serial_number,
            platform_uuid: inner.platform_uuid,
            root_disk_uuid: inner.root_disk_uuid,
            board_id: inner.board_id,
            os_build_num: inner.os_build_num,
            platform_serial_number_enc: inner.platform_serial_number_enc,
            platform_uuid_enc: inner.platform_uuid_enc,
            root_disk_uuid_enc: inner.root_disk_uuid_enc,
            rom: inner.rom,
            rom_enc: inner.rom_enc,
            mlb: inner.mlb,
            mlb_enc: inner.mlb_enc
        },
        version: copied.version,
        protocol_version: copied.protocol_version as u32,
        device_id: copied.device_id,
        icloud_ua: copied.icloud_ua,
        aoskit_version: copied.aoskit_version,
        udid: Some(generate_udid()),
    })))
}

pub async fn setup_push(config: &JoinedOSConfig, identity: &IDSNGMIdentity, state: Option<APSState>, state_path: String) -> (Arc<BufferedApsConn>, Option<PushError>) {
    let state_path = PathBuf::from_str(&state_path).unwrap().join("hw_info.plist");
    let (conn, error) = APSConnectionResource::new(config.config(), state).await;
    let buffered = BufferedApsConn::new(conn);

    let saved_identity = identity.save("openbubbles").expect("failed to save");
    if error.is_none() {
        let state = SavedHardwareState {
            push: buffered.inner().state.read().await.clone(),
            os_config: config.clone(),
            identity: saved_identity.clone(),
        };
        match plist_to_buf(&state) {
            Ok(buf) => {
                if let Err(e) = crate::persist::write_atomic(&state_path, &buf) {
                    error!("failed to persist initial hw_info.plist: {e}");
                }
            }
            Err(e) => error!("failed to serialize initial hw_info.plist: {e}"),
        }
    }

    let mut to_refresh = buffered.inner().generated_signal.subscribe();
    let reconn_conn = Arc::downgrade(buffered.inner());
    let config_ref = config.clone();
    tokio::spawn(async move {
        loop {
            match to_refresh.recv().await {
                Ok(()) => {
                    let Some(conn) = reconn_conn.upgrade() else { break };
                    // update keys
                    let state = SavedHardwareState {
                        push: conn.state.read().await.clone(),
                        os_config: config_ref.clone(),
                        identity: saved_identity.clone(),
                    };
                    match plist_to_buf(&state) {
                        Ok(buf) => {
                            if let Err(e) = crate::persist::write_atomic(&state_path, &buf) {
                                error!("failed to persist hw_info.plist in re-key task: {e}");
                            }
                        }
                        Err(e) => error!("failed to serialize hw_info.plist in re-key task: {e}"),
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    (buffered, error)
}

pub async fn make_anisette(path: String, config: &JoinedOSConfig, conn: &APSConnection) -> ArcAnisetteClient<DefaultAnisetteProvider> {
    let dir = PathBuf::from_str(&path).unwrap();

    default_provider(get_login_config(&dir, config, conn).await, dir.join("anisette_test"))
}

pub fn subscribe_conn(conn: &Arc<BufferedApsConn>) -> broadcast::Receiver<APSMessage> {
    conn.subscribe()
}

pub async fn make_idms(conn: &Arc<BufferedApsConn>) -> Arc<IdmsAuthListener> {
    IdmsAuthListener::new(conn.inner().clone()).await.into()
}

async fn get_login_config(conf_dir: &Path, conf: &JoinedOSConfig, conn: &APSConnection) -> LoginClientInfo {
    let anisette_dir = conf_dir.join("anisette_test");
    let config_path = anisette_dir.join("state.plist");

    let require_mac = if let Ok(decoded) = plist::from_file::<_, AnisetteState>(config_path) {
        matches!(decoded.provisioned, Some(ProvisionedAnisette { flavor: ProvisionedFlavor::Mac, .. }))
    } else {
        false
    };

    conf.get_gsa_config(&*conn.state.read().await, require_mac)
}

pub async fn try_auth(path: String, conf: &JoinedOSConfig, conn: &APSConnection, anisette: &ArcAnisetteClient<DefaultAnisetteProvider>, creds: Option<(String, String)>) -> anyhow::Result<(Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, LoginState)> {
    let conf_dir = PathBuf::from_str(&path).unwrap();
    info!("Here");
    let mut apple_account =
        AppleAccount::new_with_anisette(get_login_config(&conf_dir, conf, conn).await, anisette.clone())?;
    
    let result = if let Some((username, password)) = creds {
        reset_user(&path);

        let mut password_hasher = sha2::Sha256::new();
        password_hasher.update(password.as_bytes());
        let hashed_password = password_hasher.finalize();
        (username, hashed_password.to_vec())
    } else {
        let state = plist::from_file::<_, GSAConfig>(&conf_dir.join("gsa.plist"))?;
        (state.username.clone(), state.get_password()?)
    };

    let login_state = apple_account.login_email_pass(&result.0, &result.1).await?;

    info!("Here3");

    let account = Arc::new(Mutex::new(apple_account));
    
    info!("Here6");
    Ok((account, login_state))
}

pub async fn try_icloud_login(path: String, conf: &JoinedOSConfig, account: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>) -> anyhow::Result<Option<IDSUser>> {
    let pet = account.lock().await.get_pet();
    if let Some(_pet) = pet {
        info!("Here4");
        let identity = do_login(path, account, None, conf).await?;
        info!("Here5");
        
        Ok(Some(identity))
    } else {
        Ok(None)
    }
}

pub async fn do_login(path: String, account: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, finish: Option<UpdateAccountFinish>, os_config: &JoinedOSConfig) -> anyhow::Result<IDSUser> {
    let mut account = account.lock().await;
    
    let conf_dir = PathBuf::from_str(&path).unwrap();

    account.update_postdata("Apple Device", None, &["icloud", "imessage", "facetime"]).await?;
    
    let Some(_pet) = account.get_pet() else { return Err(anyhow!("No pet!")) };
    let Some(spd) = &account.spd else { return Err(anyhow!("No spd!")) };

    debug!("Got spd {:?}", spd);
    let _acname = spd.get("acname").ok_or(anyhow!("No acname!"))?.as_string().unwrap().to_string();
    let dsid = spd.get("DsPrsId").ok_or(anyhow!("No dsid!"))?.as_unsigned_integer().unwrap().to_string();
    let adsid = spd.get("adsid").ok_or(anyhow!("No adsid!"))?.as_string().unwrap();
    
    let delegates = if let Some(finish) = finish {
        finish.accept_terms(&[LoginDelegate::IDS, LoginDelegate::MobileMe], &*account, &*os_config.config()).await?
    } else {
        login_apple_delegates(&*account, None, &*os_config.config(), &[LoginDelegate::IDS, LoginDelegate::MobileMe]).await?
    };
    
    
    let gsa_config = GSAConfig {
        username: account.username.clone().unwrap(),
        encrypted_password: GSAConfig::encrypt(&account.hashed_password.clone().unwrap())?,
        postdata_done: Some(true),
    };
    crate::persist::write_atomic(&conf_dir.join("gsa.plist"), &plist_to_buf(&gsa_config)?)?;

    let sk_path = conf_dir.join("statuskit.plist");
    let sk_state = StatusKitState {
        my_key: None,
        ..plist::from_file(&sk_path).unwrap_or_default()
    };
    crate::persist::write_atomic(&sk_path, &plist_to_buf(&sk_state)?)?;
    
    let mobileme = delegates.mobileme.unwrap();
    let findmy = FindMyState::new(dsid.clone());

    let findmy_path = conf_dir.join("findmy.plist");
    if !findmy_path.exists() {
        crate::persist::write_atomic(&findmy_path, &findmy.encode()?)?;
    }

    let shared_streams = SharedStreamsState::new(dsid.clone(), &mobileme);
    if let Some(shared_streams) = shared_streams {
        let ss_path = conf_dir.join("sharedstreams.plist");
        if !ss_path.exists() {
            crate::persist::write_atomic(&ss_path, &plist_to_buf(&shared_streams)?)?;
        }
    } else {
        warn!("missing shared streams tokens!");
    }

    let cloudkitstate = CloudKitState::new(dsid.clone());
    if let Some(cloudkitstate) = cloudkitstate {
        let ck_path = conf_dir.join("cloudkit.plist");
        if !ck_path.exists() {
            crate::persist::write_atomic(&ck_path, &plist_to_buf(&cloudkitstate)?)?;
        }
    } else {
        warn!("missing cloudkit tokens!");
    }

    let keychain = KeychainClientState::new(dsid.clone(), adsid.to_string(), &mobileme);
    if let Some(keychain) = keychain {
        let kc_path = conf_dir.join("keychain.plist");
        if !kc_path.exists() {
            crate::persist::write_atomic(&kc_path, &plist_to_buf(&keychain)?)?;
        }
    } else {
        warn!("missing keychain tokens!");
    }

    debug!("Spd finish parse");

    let user = authenticate_apple(delegates.ids.unwrap(), &*os_config.config()).await?;
    Ok(user)
}

pub async fn send_2fa_to_devices(state: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, conn: &APSConnection) -> anyhow::Result<(CircleClientSession<DefaultAnisetteProvider>, LoginState, Option<String>)> {
    let account = state.lock().await;

    let spd = account.spd.as_ref().unwrap();
    let dsid = spd["DsPrsId"].as_unsigned_integer().unwrap();

    drop(account);

    let client_session = CircleClientSession::new(dsid, state.clone(), conn.get_token().await).await?;
    let sid = client_session.session_id.clone();

    Ok((client_session, LoginState::Needs2FAVerification, sid))
}

#[allow(clippy::too_many_arguments)] // Mirrors the upstream rustpush::verify_2fa signature; diverging from upstream would make re-vendoring painful.
pub async fn verify_2fa(path: String, client: &mut CircleClientSession<DefaultAnisetteProvider>, _anisette: &ArcAnisetteClient<DefaultAnisetteProvider>, os_config: &JoinedOSConfig, account: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, watcher: &mut broadcast::Receiver<APSMessage>, idms: &Arc<IdmsAuthListener>, code: String) -> anyhow::Result<(LoginState, Option<IDSUser>)> {
    client.send_code(&code).await?;

    // todo add timeout
    let mut login_state = tokio::time::timeout(Duration::from_secs(30), async {
        Ok::<_, PushError>(loop {
            let msg = watcher.recv().await.unwrap();
            if let Some(IdmsMessage::CircleRequest(c, _)) = idms.handle(msg)? {
                if let Some(state) = client.handle_circle_request(&c).await? {
                    break state;
                }
            }
        })
    }).await.map_err(|_| anyhow!("Timed Out!"))??;

    let mut user = None;
    let pet = account.lock().await.get_pet();
    if let Some(_pet) = pet {
        let identity = do_login(path, account, None, os_config).await?;
        user = Some(identity);

        // who needs extra steps when you have a PET, amirite?
        println!("confirmed login {:?}", login_state);
        if matches!(login_state, LoginState::NeedsExtraStep(_)) {
            login_state = LoginState::LoggedIn;
        }
    }

    Ok((login_state, user))
}

pub async fn get_2fa_sms_opts(state: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>) -> anyhow::Result<(Vec<TrustedPhoneNumber>, Option<LoginState>)> {
    let account = state.lock().await;
    let extras = account.get_auth_extras().await?;
    Ok((
        extras.trusted_phone_numbers,
        extras.new_state
    ))
}

pub async fn send_2fa_sms(locked: Option<CircleClientSession<DefaultAnisetteProvider>>, account: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, phone_id: u32) -> anyhow::Result<LoginState> {
    if let Some(l) = locked {
        l.cancel().await?;
    }

    let account = account.lock().await;
    Ok(account.send_sms_2fa_to_devices(phone_id).await?)
}

pub async fn verify_2fa_sms(path: String, account_mut: &Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>, _anisette: &ArcAnisetteClient<DefaultAnisetteProvider>, config: &JoinedOSConfig, body: &VerifyBody, code: String) -> anyhow::Result<(LoginState, Option<IDSUser>)> {
    let mut account = account_mut.lock().await;
    let mut login_state = account.verify_sms_2fa(code, body.clone()).await?;

    let mut user = None;
    if let Some(_pet) = account.get_pet() {
        drop(account);
        let identity = do_login(path, account_mut, None, config).await?;
        user = Some(identity);

        // who needs extra steps when you have a PET, amirite?
        println!("confirmed login {:?}", login_state);
        if matches!(login_state, LoginState::NeedsExtraStep(_)) {
            login_state = LoginState::LoggedIn;
        }
    }

    Ok((login_state, user))
}

#[derive(Serialize, Deserialize)]
struct GSAConfig {
    username: String,
    encrypted_password: Data,
    postdata_done: Option<bool>,
}

impl GSAConfig {
    fn get_password(&self) -> Result<Vec<u8>, PushError> {
        let key = AesKeystoreKey::ensure("gsa:password", 256, KeystoreAccessRules {
            block_modes: vec![EncryptMode::Gcm],
            can_encrypt: true,
            can_decrypt: true,
            ..Default::default()
        })?;
        let encoded = key.decrypt(self.encrypted_password.as_ref(), &EncryptMode::Gcm)?;
        Ok(encoded)
    }

    fn encrypt(password: &[u8]) -> Result<Data, PushError> {
        let key = AesKeystoreKey::ensure("gsa:password", 256, KeystoreAccessRules {
            block_modes: vec![EncryptMode::Gcm],
            can_encrypt: true,
            can_decrypt: true,
            ..Default::default()
        })?;
        let encoded = key.encrypt(password, &mut EncryptMode::Gcm)?;
        Ok(encoded.into())
    }
}

fn reset_user(path: &str) {
    let dir = PathBuf::from_str(path).unwrap();

    let _ = std::fs::remove_file(dir.join("gsa.plist"));
    let _ = std::fs::remove_file(dir.join("findmy.plist"));
    let _ = std::fs::remove_file(dir.join("facetime.plist"));
    let _ = std::fs::remove_file(dir.join("cloudkit.plist"));
    let _ = std::fs::remove_file(dir.join("keychain.plist"));
    let _ = std::fs::remove_file(dir.join("passwords.plist"));
    let _ = std::fs::remove_file(dir.join("sharedstreams.plist"));

    let sk_path = dir.join("statuskit.plist");
    match plist_to_buf(&StatusKitState {
        my_key: None,
        ..plist::from_file(&sk_path).unwrap_or_default()
    }) {
        Ok(buf) => {
            if let Err(e) = crate::persist::write_atomic(&sk_path, &buf) {
                error!("failed to reset statuskit.plist: {e}");
            }
        }
        Err(e) => error!("failed to serialize statuskit.plist during reset: {e}"),
    }
}

/// Wipe the persisted login so the next launch goes through onboarding.
/// `restore_session` gates on `hw_info.plist` + `id.plist`; the rest is cleared
/// so a fresh sign-in doesn't reuse stale Apple-account/anisette state.
pub fn clear_session(path: &str) {
    let dir = PathBuf::from_str(path).unwrap_or_default();
    for f in [
        "hw_info.plist",
        "id.plist",
        "gsa.plist",
        "keystore.plist",
        "keychain.plist",
        "statuskit.plist",
        "findmy.plist",
        "facetime.plist",
        "cloudkit.plist",
        "sharedstreams.plist",
        "passwords.plist",
    ] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let _ = std::fs::remove_dir_all(dir.join("anisette_test"));
}

pub async fn register_ids(path: String, config: &JoinedOSConfig, aps: &APSConnection, identity: &IDSNGMIdentity, mut users: Vec<IDSUser>) -> anyhow::Result<(Option<Vec<IDSUser>>, Option<SupportAlert>)> {
    let dir = PathBuf::from_str(&path).unwrap();

    if let Err(err) = register(&*config.config(), &*aps.state.read().await, &[&MADRID_SERVICE, &MULTIPLEX_SERVICE, &FACETIME_SERVICE, &VIDEO_SERVICE], &mut users, identity).await {
        return if let PushError::CustomerMessage(support) = err {
            Ok((None, Some(support)))
        } else {
            Err(anyhow!(err))
        }
    }
    let id_path = dir.join("id.plist");
    crate::persist::write_atomic(&id_path, &plist_to_buf(&users)?)?;

    Ok((Some(users), None))
}

pub async fn make_imclient(path: String, conn: &APSConnection, users: &[IDSUser], identity: &IDSNGMIdentity) -> Arc<IMClient> {
    let dir = PathBuf::from_str(&path).unwrap();
    let id_path = dir.join("id.plist");

    let incident_path = dir.join("incident");
    if !incident_path.exists() {
        if plist::from_file::<_, KeyCache>(dir.join("id_cache.plist")).is_ok() {
            let _ = fs::File::create(dir.join("incident_affected"));
        }
        let _ = fs::File::create(incident_path);
    }

    Arc::new(IMClient::new(conn.clone(), users.to_vec(), identity.clone(),
    &[&MADRID_SERVICE, &MULTIPLEX_SERVICE, &FACETIME_SERVICE, &VIDEO_SERVICE], dir.join("id_cache.plist"), conn.os_config.clone(), Box::new(move |updated_keys| {
        println!("updated keys!!!");
        match plist_to_buf(&updated_keys) {
            Ok(buf) => {
                if let Err(e) = crate::persist::write_atomic(&id_path, &buf) {
                    error!("failed to persist id.plist in key-update callback: {e}");
                }
            }
            Err(e) => error!("failed to serialize id.plist in key-update callback: {e}"),
        }
    })).await)
}

pub async fn get_handles(state: &Arc<IMClient>) -> anyhow::Result<Vec<String>> {
    Ok(state.identity.get_handles().await.to_vec())
}
