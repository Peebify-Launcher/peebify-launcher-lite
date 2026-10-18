// ------------ Game Profiles ------------
// The list of supported games and everything the launcher needs to know about each one: names, executables, download sources, install type, mod support and so on.
// Other files look a game up here by id instead of hardcoding anything about it. Also checks where a game really lives
// from the folder someone picked.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Value};

pub const GAME_IDS: [&str; 15] = [
    "wuwa", "zzz", "hsr", "nte", "endfield", "genshin", "hi3", "pgr", "gf2", "gf1", "re1999", "bd2",
    "arknights", "bluearchive", "dna",
];
pub const DEFAULT_GAME_ID: &str = "wuwa";
pub const VOICE_LANGUAGES: [&str; 4] = ["en-us", "ja-jp", "ko-kr", "zh-cn"];

pub fn is_known_game_id(id: &str) -> bool {
    GAME_IDS.contains(&id)
}

fn voice_map() -> &'static parking_lot::Mutex<std::collections::HashMap<String, String>> {
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, String>>> =
        OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

pub fn set_audio_language(profile_id: &str, language: &str) {
    let mut map = voice_map().lock();
    if VOICE_LANGUAGES.contains(&language) {
        log::info!("[voice] {profile_id} voice pack set to {language}");
        map.insert(profile_id.to_string(), language.to_string());
    } else {
        map.remove(profile_id);
    }
}

pub fn audio_language(profile_id: &str) -> String {
    voice_map()
        .lock()
        .get(profile_id)
        .cloned()
        .unwrap_or_else(|| super::sophon::HOYO_AUDIO_LANGUAGE.to_string())
}

fn saved_voice_language(app: &tauri::AppHandle, key: &str) -> Option<String> {
    super::backend(app)
        .config
        .get(key)
        .as_str()
        .filter(|lang| VOICE_LANGUAGES.contains(lang))
        .map(str::to_string)
}

async fn voice_language_on_disk(
    profile: &Value,
    dir: &std::path::Path,
    build: Option<&super::sophon::Build>,
) -> Option<String> {
    let probe = dir.to_path_buf();
    let has_files = tauri::async_runtime::spawn_blocking(move || {
        std::fs::read_dir(&probe).is_ok_and(|mut entries| entries.next().is_some())
    })
    .await
    .unwrap_or(false);
    if !has_files {
        return None;
    }

    let fetched;
    let build = match build {
        Some(build) => build,
        None => {
            let fetch = async {
                let auth = super::sophon::cached_branch_auth_for_profile(profile).await?;
                super::sophon::fetch_build(&auth).await
            };
            match fetch.await {
                Ok(build) => {
                    fetched = build;
                    &fetched
                }
                Err(e) => {
                    log::warn!(
                        "[voice] {}: could not read the build to find the voice pack: {e}",
                        profile_id(profile)
                    );
                    return None;
                }
            }
        }
    };
    match super::sophon::detect_voice_language(dir, build, &VOICE_LANGUAGES).await {
        Ok(found) => found,
        Err(e) => {
            log::warn!(
                "[voice] {}: could not check the voice pack on disk: {e}",
                profile_id(profile)
            );
            None
        }
    }
}

pub(crate) async fn resolve_audio_language(
    app: &tauri::AppHandle,
    profile: &Value,
    dir: &std::path::Path,
    build: Option<&super::sophon::Build>,
) -> String {
    let id = profile_id(profile);
    if install_mode(profile) != Some("sophon") {
        return audio_language(id);
    }
    let key = format!("games.{id}.voicePackLanguage");
    if let Some(lang) = saved_voice_language(app, &key) {
        return lang;
    }

    let recorded_dir = dir.to_path_buf();
    let recorded = tauri::async_runtime::spawn_blocking(move || {
        super::validator::Record::load(&recorded_dir)
            .groups
            .into_iter()
            .find(|lang| VOICE_LANGUAGES.contains(&lang.as_str()))
    })
    .await
    .ok()
    .flatten();
    let (found, source) = match recorded {
        Some(lang) => (Some(lang), "Peebify's install record"),
        None => (
            voice_language_on_disk(profile, dir, build).await,
            "the files on disk",
        ),
    };
    let Some(lang) = found else {
        return audio_language(id);
    };

    if let Some(saved) = saved_voice_language(app, &key) {
        return saved;
    }
    log::info!("[voice] {id}: found the {lang} voice pack from {source}.");
    super::config::set_config_value(app, &key, json!(lang));
    set_audio_language(id, &lang);
    super::engine::invalidate_preview(id);
    use tauri::Emitter;
    let _ = app.emit("settings-changed", json!({ "key": key, "value": lang }));
    lang
}

fn content_tag_map() -> &'static parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>
{
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>> =
        OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

pub fn set_content_tags(profile_id: &str, tags: Option<Vec<String>>) {
    let mut map = content_tag_map().lock();
    match tags {
        Some(list) => {
            log::info!("[content] {profile_id} optional packs set to {list:?}");
            map.insert(profile_id.to_string(), list);
        }
        None => {
            map.remove(profile_id);
        }
    }
}

pub fn content_tags(profile_id: &str) -> Option<Vec<String>> {
    content_tag_map().lock().get(profile_id).cloned()
}

// ------------ Profile Table ------------
// The profile of every supported game, written out as one big JSON value that is built once on first use. Each profile gets its "id" from its key.
fn profiles() -> &'static Value {
    static PROFILES: OnceLock<Value> = OnceLock::new();
    PROFILES.get_or_init(|| {
        let mut table = json!({
            "wuwa": {
                "wallpaperSlug": "wuthering-waves",
                "displayName": "Wuthering Waves",

                "supportsManagedInstall": true,
                "gameConfigUrl": "https://prod-alicdn-gamestarter.kurogame.com/launcher/game/50004_P7xcUZnEr1AXIGON25E6KjpOgTlVrg6e/G153/official/index.json",
                "extraMirrors": ["https://hw-pcdownload-aliyun.aki-game.net"],

                "executableName": "Wuthering Waves.exe",
                "clientProcessName": "Client-Win64-Shipping.exe",

                "steamAppId": "3513350",
                "steamAppIdFiles": [["Client", "Binaries", "Win64", "steam_appid.txt"]],

                "graphicsApiArgs": { "dx11": [], "dx12": ["-dx12"] },
                "graphicsApiDefault": "dx12",

                "resourceQualityArgs": { "sd": ["-krqlv=sd"], "hd": ["-krqlv=hd"], "uhd": ["-krqlv=uhd"] },
                "resourceQualityDefault": "hd",
                "resourceQualityDir": ["Client", "Content", "{QUALITY}"],

                "mods": {
                    "variant": "wwmi",
                    "loaderMode": "inject",
                    "forceDx11Args": ["-dx11"],
                    "gameBananaId": 20357,
                    "gameSettings": "wuwa",
                    "unwantedTriggers": ["ib", "vb0"],
                    "ibTriggerReplacement": "$\\WWMIv1\\enable_ib_callbacks = 1",
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 1, "track_texture_updates": 1, "track_region_hashes": 0, "allow_buffer_resize": 1 },
                        "System": { "dll_initialization_delay": 500 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "Wuthering Waves Official",
                            "url": "https://wutheringwaves.kurogames.com/en/main"
                        },
                        {
                            "name": "Wuthering Waves News",
                            "url": "https://wutheringwaves.kurogames.com/en/main#news"
                        }
                    ],
                    "community": [
                        { "name": "Wuthering Waves Tracker", "url": "https://wuwatracker.com/" },
                        { "name": "Wuthering Waves Map", "url": "https://wuthering-waves-map.appsample.com/" },
                        { "name": "Wuthering Waves Builds", "url": "https://game8.co/games/Wuthering-Waves/archives/457465" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/wutheringwaves",
                    "youtube": "https://www.youtube.com/c/WutheringWaves",
                    "x": "https://twitter.com/Wuthering_Waves",
                    "lunite": "https://payment.kurogame-service.com/pay/wutheringwaves/"
                }
            },

            "zzz": {
                "wallpaperSlug": "zenless-zone-zero",
                "displayName": "Zenless Zone Zero",

                "supportsManagedInstall": true,
                "installMode": "sophon",
                "hoyoBiz": "nap_global",
                "hoyoGameId": "U5hbdsT9W7",

                "executableName": "ZenlessZoneZero.exe",
                "clientProcessName": "ZenlessZoneZero.exe",

                "steamAppId": "4162040",
                "steamAppIdFiles": [["steam_appid.txt"]],

                "graphicsApiArgs": { "dx11": [], "dx12": ["-use-d3d12"] },
                "graphicsApiDefault": "dx11",

                "mods": {
                    "variant": "zzmi",
                    "loaderMode": "hook",
                    "forceDx11Args": ["-use-d3d11"],
                    "gameBananaId": 19567,
                    "gameSettings": "zzz",
                    "bundledLibraries": ["Core", "ZZMI", "Libraries"],
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 0, "track_texture_updates": 0, "track_region_hashes": 0, "allow_buffer_resize": 1 },
                        "System": { "dll_initialization_delay": 0 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "Zenless Zone Zero Official",
                            "url": "https://zenless.hoyoverse.com/"
                        },
                        {
                            "name": "Zenless Zone Zero News",
                            "url": "https://zenless.hoyoverse.com/news"
                        }
                    ],
                    "community": [
                        { "name": "Zenless Zone Zero Builds", "url": "https://docs.google.com/spreadsheets/d/e/2PACX-1vTj2PaPq6Py_1B5fsOPj_Moc-tN_7mut7fICczI6lz1njyEIAInTnfB7lAraX4pYCRGNbaHGlIbFZ90/pubhtml" },
                        { "name": "Interknot Network", "url": "https://interknot-network.com/" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.com/invite/zenlesszonezero",
                    "youtube": "https://www.youtube.com/@ZZZ_Official",
                    "x": "https://twitter.com/ZZZ_EN",
                    "hoyolab": "https://www.hoyolab.com/accountCenter/postList?id=219270333"
                }
            },

            "hsr": {
                "wallpaperSlug": "honkai-star-rail",
                "displayName": "Honkai: Star Rail",

                "supportsManagedInstall": true,
                "installMode": "sophon",
                "hoyoBiz": "hkrpg_global",
                "hoyoGameId": "4ziysqXOQ8",

                "executableName": "StarRail.exe",
                "clientProcessName": "StarRail.exe",

                "mods": {
                    "variant": "srmi",
                    "loaderMode": "hook",
                    "gameBananaId": 18366,
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 0, "track_texture_updates": 0, "track_region_hashes": 0, "track_implicit_index_buffers": 1, "allow_buffer_resize": 1 },
                        "System": { "dll_initialization_delay": 0 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "Honkai: Star Rail Official",
                            "url": "https://hsr.hoyoverse.com/"
                        },
                        {
                            "name": "Honkai: Star Rail News",
                            "url": "https://hsr.hoyoverse.com/en/news"
                        }
                    ],
                    "community": [
                        { "name": "Honkai: Star Rail Builds", "url": "https://game8.co/games/Honkai-Star-Rail/archives/404256" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/honkaistarrail",
                    "youtube": "https://www.youtube.com/@HonkaiStarRail",
                    "x": "https://twitter.com/HonkaiStarRail",
                    "hoyolab": "https://www.hoyolab.com/accountCenter/postList?id=172534910"
                }
            },

            "nte": {
                "wallpaperSlug": "neverness-to-everness",
                "displayName": "Neverness to Everness",

                "supportsManagedInstall": true,
                "installMode": "netease",

                "nteResUrls": [
                    "https://ntecdn1.perfectworld.com/clientRes",
                    "https://ntecdn2.perfectworld.com/clientRes"
                ],
                "nteLauncherUrls": [
                    "https://ntecdn1.perfectworld.com/hd/publish_PC/launcher",
                    "https://ntecdn2.perfectworld.com/hd/publish_PC/launcher"
                ],
                "nteBranch": "publish_PC",
                "nteAppId": "3000001",

                "installRootMarker": ["Client", "WindowsNoEditor", "HT", "Binaries", "Win64", "HTGame.exe"],
                "launchCandidates": [
                    { "path": ["NTEGlobal", "NTEGlobalGame.exe"], "args": ["/launcher"] },
                    { "path": ["Client", "WindowsNoEditor", "HT", "Binaries", "Win64", "HTGame.exe"] }
                ],
                "executableName": "NTEGlobalGame.exe",
                "clientProcessName": "HTGame.exe",
                "exitCompanions": ["NTEGlobalGame.exe", "NTEGlobalBrowser.exe", "NTEGlobalWebBooster.exe"],
                "frontEnd": {
                    "readyLog": ["NTEGlobal", "UserData", "Log", "NTEGlobalGame.log"],
                    "readyLine": "all ready, wait for start game",
                    "button": "TextButton_"
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "NTE Official",
                            "url": "https://nte.perfectworld.com/en/"
                        },
                        {
                            "name": "NTE News",
                            "url": "https://nte.perfectworld.com/en/article/news/gamenews/index.html"
                        }
                    ],
                    "community": [
                        { "name": "Neverness to Everness Builds", "url": "https://game8.co/games/Neverness-to-Everness/archives/596186" }
                    ]
                },

                "socialUrls": {}
            },

            "endfield": {
                "wallpaperSlug": "arknights-endfield",
                "displayName": "Arknights: Endfield",

                "supportsManagedInstall": true,
                "installMode": "hypergryph",

                "hgApiUrl": "https://launcher.gryphline.com/api/proxy/batch_proxy",
                "hgWebApiUrl": "https://launcher.gryphline.com/api/proxy/web/batch_proxy",
                "hgAppCode": "YDUTE5gscDZ229CW",
                "hgChannel": "6",
                "hgSubChannel": "6",
                "hgSeq": "3",
                "hgLanguage": "en-us",
                "hgLauncherAppCode": "TiaytKBUIEdoEwRT",

                "executableName": "Endfield.exe",
                "clientProcessName": "Endfield.exe",
                "launchParent": "Games.exe",

                "steamAppId": "4732690",

                "mods": {
                    "variant": "efmi",
                    "loaderMode": "inject",
                    "forceDx11Args": ["-force-d3d11"],
                    "gameBananaId": 21842,
                    "unwantedTriggers": ["ib"],
                    "unwantedModFiles": ["VSCheck.ini"],
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 0, "track_texture_updates": 0, "track_region_hashes": 1, "track_implicit_index_buffers": 1, "allow_buffer_resize": 0 },
                        "System": { "dll_initialization_delay": 0 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "Arknights: Endfield Official",
                            "url": "https://endfield.gryphline.com/en-us#home"
                        },
                        {
                            "name": "Arknights: Endfield News",
                            "url": "https://endfield.gryphline.com/en-us#notice"
                        }
                    ],
                    "community": []
                },

                "socialUrls": {
                    "discord": "https://discord.com/invite/akendfield",
                    "youtube": "https://youtube.com/@arknightsendfielden",
                    "x": "https://x.com/AKEndfield"
                }
            },

            "genshin": {
                "wallpaperSlug": "genshin-impact",
                "displayName": "Genshin Impact",

                "supportsManagedInstall": true,
                "installMode": "sophon",
                "hoyoBiz": "hk4e_global",
                "hoyoGameId": "gopR6Cufr3",

                "executableName": "GenshinImpact.exe",
                "clientProcessName": "GenshinImpact.exe",

                "fpsUnlock": true,

                "mods": {
                    "variant": "gimi",
                    "loaderMode": "hook",
                    "gameBananaId": 8552,
                    "gameSettings": "genshin",
                    "bundledLibraries": ["Core", "GIMI", "Libraries"],
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 0, "track_texture_updates": 0, "track_region_hashes": 0, "allow_buffer_resize": 1 },
                        "System": { "dll_initialization_delay": 0 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "communityTools": {
                    "official": [
                        {
                            "name": "Genshin Impact Official",
                            "url": "https://genshin.hoyoverse.com/"
                        },
                        {
                            "name": "Genshin Impact News",
                            "url": "https://genshin.hoyoverse.com/en/news"
                        }
                    ],
                    "community": [
                        { "name": "Genshin Interactive Map", "url": "https://act.hoyolab.com/ys/app/interactive-map/index.html" },
                        { "name": "Genshin Impact Builds", "url": "https://genshin-builds.com/" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/genshinimpact",
                    "youtube": "https://www.youtube.com/@GenshinImpact",
                    "x": "https://twitter.com/GenshinImpact",
                    "hoyolab": "https://www.hoyolab.com/accountCenter/postList?id=143134221"
                }
            },

            "hi3": {
                "wallpaperSlug": "honkai-impact-3rd",
                "displayName": "Honkai Impact 3rd",

                "supportsManagedInstall": true,
                "installMode": "sophon",
                "hoyoGameId": "5TIVvvcwtM",
                "hoyoBiz": "bh3_global",

                "executableName": "BH3.exe",
                "clientProcessName": "BH3.exe",

                "mods": {
                    "variant": "himi",
                    "loaderMode": "hook",
                    "gameBananaId": 10349,
                    "experimental": true,
                    "modIniDefaults": {
                        "Rendering": { "texture_hash": 0, "track_texture_updates": 0, "track_region_hashes": 0, "allow_buffer_resize": 1 },
                        "System": { "dll_initialization_delay": 0 },
                        "Logging": { "show_warnings": 0 }
                    }
                },

                "steamAppId": "1671200",

                "communityTools": {
                    "official": [
                        {
                            "name": "Honkai Impact 3rd Official",
                            "url": "https://honkaiimpact3.hoyoverse.com/"
                        },
                        {
                            "name": "Honkai Impact 3rd News",
                            "url": "https://honkaiimpact3.hoyoverse.com/news"
                        }
                    ],
                    "community": [
                        { "name": "Honkai Impact 3rd Builds", "url": "https://game8.co/games/Honkai-Impact-3rd" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/honkaiimpact3",
                    "youtube": "https://www.youtube.com/@HonkaiImpact3rd",
                    "x": "https://twitter.com/HonkaiImpact3",
                    "hoyolab": "https://www.hoyolab.com/accountCenter/postList?id=73565430"
                }
            },

            "pgr": {
                "wallpaperSlug": "punishing-gray-raven",
                "displayName": "Punishing: Gray Raven",

                "supportsManagedInstall": true,
                "gameConfigUrl": "https://prod-alicdn-gamestarter.kurogame.com/launcher/game/G143/50015_LWdk9D2Ep9mpJmqBZZkcPBU2YNraEWBQ/index.json",
                "extraMirrors": ["https://zspms-alicdn-gamestarter.kurogame.net"],
                "newsSource": "pgr",

                "executableName": "PGR.exe",
                "clientProcessName": "PGR.exe",

                "steamAppId": "4125930",

                "graphicsApiArgs": { "dx11": ["-force-d3d11"], "dx12": ["-force-d3d12"] },
                "graphicsApiDefault": "dx11",

                "communityTools": {
                    "official": [
                        {
                            "name": "Punishing: Gray Raven Official",
                            "url": "https://pgr.kurogame.net/"
                        },
                        {
                            "name": "Punishing: Gray Raven News",
                            "url": "https://pgr.kurogame.net/news"
                        }
                    ],
                    "community": [
                        { "name": "Punishing: Gray Raven Builds", "url": "https://grayravens.com/wiki/Guides" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/pgr",
                    "youtube": "https://www.youtube.com/@PunishingGrayRaven",
                    "x": "https://twitter.com/PGR_Global"
                }
            },

            "gf2": {
                "wallpaperSlug": "girls-frontline-2",
                "displayName": "Girls' Frontline 2: Exilium",

                "supportsManagedInstall": true,
                "installMode": "gf2",

                "gf2ConfigUrl": "https://gf2-launcher-us.sunborngame.com/flexible_config?version=1.0.2",
                "gf2ClientPackage": "GF2_Exilium_Origin.zip",

                "executableName": "GF2_Exilium.exe",
                "clientProcessName": "GF2_Exilium.exe",

                "steamAppId": "3347400",

                "communityTools": {
                    "official": [
                        {
                            "name": "Girls' Frontline 2 Official",
                            "url": "https://gf2exilium.sunborngame.com/"
                        },
                        {
                            "name": "Girls' Frontline 2 News",
                            "url": "https://gf2exilium.sunborngame.com/main/noticeMore"
                        }
                    ],
                    "community": [
                        { "name": "IOP Wiki", "url": "https://iopwiki.com/wiki/Girls%27_Frontline_2:_Exilium" },
                        { "name": "Girls' Frontline 2 Builds", "url": "https://game8.co/games/Girls-Frontline-2-Exilium" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/gfl2",
                    "youtube": "https://www.youtube.com/@GFL2EXILIUM",
                    "x": "https://x.com/gfl2exilium_en"
                }
            },

            "gf1": {
                "wallpaperSlug": "girls-frontline",
                "displayName": "Girls' Frontline",
                "supportsManagedInstall": false,
                "newsSource": "gf1",

                "executableName": "GirlsFrontLine.exe",
                "clientProcessName": "GirlsFrontLine.exe",
                "exitCompanions": ["ZFGameBrowser.exe"],

                "steamAppId": "3887700",

                "communityTools": {
                    "official": [
                        {
                            "name": "Girls' Frontline Official",
                            "url": "https://gf.sunborngame.com/"
                        },
                        {
                            "name": "Girls' Frontline News",
                            "url": "https://store.steampowered.com/news/app/3887700"
                        }
                    ],
                    "community": [
                        { "name": "IOP Wiki", "url": "https://iopwiki.com/wiki/Girls%27_Frontline" },
                        { "name": "Girls' Frontline Guides", "url": "https://iopwiki.com/wiki/Category:Guides" }
                    ]
                },

                "socialUrls": {
                    "x": "https://x.com/GirlsFrontlineE"
                }
            },

            "re1999": {
                "wallpaperSlug": "reverse-1999",
                "displayName": "Reverse: 1999",

                "supportsManagedInstall": true,
                "installMode": "bluepoch",

                "maxPathTail": 200,

                "bpHotUpdateUrls": [
                    "https://hotupdate-hw.sl916.com",
                    "https://hotupdate-bak-hw.sl916.com"
                ],
                "bpActivityUrls": [
                    "https://launcher-hw.sl916.com",
                    "https://launcher-bak-hw.sl916.com"
                ],
                "extraMirrors": ["https://download.bluepoch.com", "https://download-bak.bluepoch.com"],
                "bpGameId": "60001",
                "bpChannelId": 200,
                "bpSubChannelId": "6001",
                "bpOsType": 2,
                "bpEnvType": 4,
                "bpLang": "en",
                "bpNewsUrl": "https://re1999.bluepoch.com/",

                "executableName": "reverse1999.exe",
                "clientProcessName": "reverse1999.exe",
                "exitCompanions": ["ZFGameBrowser.exe", "UnityCrashHandler64.exe"],

                "communityTools": {
                    "official": [
                        {
                            "name": "Reverse: 1999 Official",
                            "url": "https://re1999.bluepoch.com/"
                        },
                        {
                            "name": "Reverse: 1999 News",
                            "url": "https://re1999.bluepoch.com/#news"
                        }
                    ],
                    "community": [
                        { "name": "Reverse: 1999 Wiki", "url": "https://reverse1999.fandom.com/wiki/Reverse:1999_Wiki" },
                        { "name": "Reverse: 1999 Builds", "url": "https://game8.co/games/Reverse-1999" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/reverse1999",
                    "youtube": "https://www.youtube.com/@Reverse1999",
                    "x": "https://twitter.com/Reverse1999_GL"
                }
            },

            "bd2": {
                "wallpaperSlug": "brown-dust-2",
                "displayName": "Brown Dust II",

                "supportsManagedInstall": true,
                "installMode": "bd2",

                "bdStarterConfigUrl": "https://pc.bd2.pmang.cloud/browndust2starter/starter/config/launcher_g.json",
                "bdGameKey": "10000002",
                "bdNewsApiUrl": "https://webapi.browndust2.com/api",
                "bdNewsUrl": "https://www.browndust2.com",
                "bdNewsLocale": "en-us",

                "executableName": "BrownDust II.exe",
                "clientProcessName": "BrownDust II.exe",
                "exitCompanions": ["UnityCrashHandler64.exe"],

                "communityTools": {
                    "official": [
                        {
                            "name": "Brown Dust II Official",
                            "url": "https://www.browndust2.com/en-us/"
                        },
                        {
                            "name": "Brown Dust II News",
                            "url": "https://www.browndust2.com/en-us/news?page=0&type=all"
                        }
                    ],
                    "community": [
                        { "name": "Brown Dust 2 Database", "url": "https://dotgg.gg/brown-dust-2/" },
                        { "name": "Brown Dust 2 Tier Lists", "url": "https://dotgg.gg/brown-dust-2/tier-lists/" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.com/invite/qMbpbvWwja",
                    "youtube": "https://www.youtube.com/channel/UCmnj4VhKgycXSq3-GrQhhgQ"
                }
            },

            "arknights": {
                "wallpaperSlug": "arknights",
                "displayName": "Arknights",

                "supportsManagedInstall": true,
                "installMode": "yostar",

                "ysApiUrl": "https://api-launcher-en.yo-star.com",
                "ysGameTag": "Arknights_EN",
                "ysLauncherVersion": "1.8.1",
                "ysSignKey": "DE7108E9B2842FD460F4777702727869",
                "ysNewsUrl": "https://www.arknights.global/news",
                "ysNewsApi": "https://www.arknights.global/api/resource/news",

                "executableName": "Arknights.exe",
                "clientProcessName": "Arknights.exe",
                "exitCompanions": ["UnityCrashHandler64.exe"],

                "graphicsApiArgs": { "dx11": ["-force-d3d11"], "dx12": ["-force-d3d12"] },
                "graphicsApiDefault": "dx11",

                "communityTools": {
                    "official": [
                        {
                            "name": "Arknights Official",
                            "url": "https://www.arknights.global/"
                        },
                        {
                            "name": "Arknights News",
                            "url": "https://www.arknights.global/news"
                        }
                    ],
                    "community": [
                        { "name": "Arknights Terra Wiki", "url": "https://arknights.wiki.gg/" },
                        { "name": "Penguin Statistics", "url": "https://penguin-stats.io/" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.com/invite/arknights",
                    "youtube": "https://www.youtube.com/channel/UCR0J2NYGuC8epsa1O4DMmXQ",
                    "x": "https://x.com/ArknightsEN"
                }
            },
            "bluearchive": {
                "wallpaperSlug": "blue-archive",
                "displayName": "Blue Archive",
                "supportsManagedInstall": false,
                "newsSource": "nexon",

                "nexonForumUrl": "https://forum.nexon.com",
                "nexonForumAlias": "bluearchive-en",
                "nexonCountryCode": "US",
                "nexonNoticeBoards": ["3028", "3219"],
                "nexonNewsBoards": ["3217", "3218"],

                "executableName": "BlueArchive.exe",
                "clientProcessName": "BlueArchive.exe",

                "steamAppId": "3557620",

                "communityTools": {
                    "official": [
                        {
                            "name": "Blue Archive Official",
                            "url": "https://bluearchive.nexon.com/home"
                        },
                        {
                            "name": "Blue Archive News",
                            "url": "https://forum.nexon.com/bluearchive-en/board_list?board=3028"
                        }
                    ],
                    "community": [
                        { "name": "SchaleDB", "url": "https://schaledb.com/" },
                        { "name": "Blue Archive Wiki", "url": "https://bluearchive.wiki/" }
                    ]
                },

                "socialUrls": {
                    "discord": "https://discord.gg/bluearchiveglobal",
                    "x": "https://x.com/en_BlueArchive"
                }
            },
            "dna": {
                "wallpaperSlug": "duet-night-abyss",
                "displayName": "Duet Night Abyss",

                "supportsManagedInstall": true,
                "installMode": "dna",

                "dnaCdns": [
                    "https://pan01-cdn-ali-jp.dna-panstudio.com",
                    "https://pan01-cdn-eo-jp.dna-panstudio.com",
                    "https://pan01-cdn-aws-jp.dna-panstudio.com",
                    "https://pan01-cdn-hs-jp.dna-panstudio.com"
                ],
                "dnaPackagePath": "Packages/Global/WindowsNoEditor/PC_OBT_Global_Pub",
                "dnaSiteUrl": "https://duetnightabyss.dna-panstudio.com",
                "dnaNewsCategory": "860",
                "dnaNewsLanguage": "EN",

                "executableName": "EM.exe",
                "clientProcessName": "EM-Win64-Shipping.exe",

                "graphicsApiArgs": { "dx11": ["-dx11"], "dx12": [] },
                "graphicsApiDefault": "dx12",

                "communityTools": {
                    "official": [
                        {
                            "name": "Duet Night Abyss Official",
                            "url": "https://duetnightabyss.dna-panstudio.com/#/home"
                        },
                        {
                            "name": "Duet Night Abyss News",
                            "url": "https://duetnightabyss.dna-panstudio.com/#/news"
                        }
                    ],
                    "community": []
                },

                "socialUrls": {
                    "discord": "https://discord.com/invite/CuYCcPhKfA",
                    "youtube": "https://www.youtube.com/@DuetNightAbyss_Official",
                    "x": "https://x.com/DNAbyss_EN"
                }
            }
        });
        for (id, profile) in table.as_object_mut().expect("the table is an object") {
            profile["id"] = json!(id);
        }
        table
    })
}

// ------------ Profile Lookups ------------
// Small getters that read one field out of a profile, so callers do not poke at the raw JSON.
pub fn known_profile(id: &str) -> Option<&'static Value> {
    profiles().get(id)
}

pub fn profile(id: &str) -> &'static Value {
    match known_profile(id) {
        Some(p) => p,
        None => {
            log::warn!("game_profiles: unknown game id \"{id}\", falling back to WUWA.");
            profiles()
                .get(DEFAULT_GAME_ID)
                .expect("WUWA profile must exist")
        }
    }
}

pub fn is_managed(profile: &Value) -> bool {
    profile
        .get("supportsManagedInstall")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn install_mode(profile: &Value) -> Option<&str> {
    profile.get("installMode").and_then(Value::as_str)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    Default,
    Sophon,
    Netease,
    Hypergryph,
    Gf2,
    Bluepoch,
    Bd2,
    Dna,
    Yostar,
    Unknown,
}

impl InstallMode {
    pub fn of(profile: &Value) -> Self {
        match install_mode(profile) {
            None => Self::Default,
            Some("sophon") => Self::Sophon,
            Some("netease") => Self::Netease,
            Some("hypergryph") => Self::Hypergryph,
            Some("gf2") => Self::Gf2,
            Some("bluepoch") => Self::Bluepoch,
            Some("bd2") => Self::Bd2,
            Some("dna") => Self::Dna,
            Some("yostar") => Self::Yostar,
            Some(_) => Self::Unknown,
        }
    }
}

pub fn profile_str<'a>(profile: &'a Value, key: &str) -> &'a str {
    profile.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

pub fn required_str<'a>(profile: &'a Value, key: &str) -> Result<&'a str, String> {
    match profile_str(profile, key) {
        "" => Err(format!("{} is missing its {key} setting.", display_name(profile))),
        value => Ok(value),
    }
}

pub fn display_name(profile: &Value) -> &str {
    profile["displayName"].as_str().unwrap_or("")
}

pub fn profile_id(profile: &Value) -> &str {
    profile["id"].as_str().unwrap_or(DEFAULT_GAME_ID)
}

pub fn executable_name(profile: &Value) -> &str {
    profile["executableName"].as_str().unwrap_or("")
}

pub fn data_dir(install_path: &Path, executable_name: &str) -> PathBuf {
    install_path.join(format!("{}_Data", executable_name.trim_end_matches(".exe")))
}

pub fn install_folder_name(profile: &Value) -> String {
    let name = display_name(profile);
    let name = if name.is_empty() {
        profile_id(profile)
    } else {
        name
    };
    name.chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect::<String>()
        .trim()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string()
}

pub const DEFAULT_MAX_PATH_TAIL: usize = 160;

pub fn max_path_tail(profile: &Value) -> usize {
    profile
        .get("maxPathTail")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_PATH_TAIL)
}

pub fn client_process_name(profile: &Value) -> &str {
    profile
        .get("clientProcessName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| executable_name(profile))
}

pub fn resource_quality_default(profile: &Value) -> Option<&str> {
    profile.get("resourceQualityArgs")?;
    profile.get("resourceQualityDefault").and_then(Value::as_str)
}

pub fn install_root_marker(profile: &Value) -> Option<Vec<String>> {
    string_array(profile.get("installRootMarker")?)
}

pub fn exit_companions(profile: &Value) -> Vec<String> {
    profile
        .get("exitCompanions")
        .and_then(string_array)
        .unwrap_or_default()
}

pub fn launch_parent(profile: &Value) -> Option<&str> {
    profile.get("launchParent")?.as_str().filter(|name| !name.is_empty())
}

pub struct FrontEnd {
    pub ready_log: Vec<String>,
    pub ready_line: String,
    pub button: String,
}

pub fn front_end(profile: &Value, executable: &std::path::Path) -> Option<FrontEnd> {
    let spec = profile.get("frontEnd")?;
    let file_name = executable.file_name()?.to_str()?;
    if !file_name.eq_ignore_ascii_case(executable_name(profile)) {
        return None;
    }
    let text = |key: &str| {
        spec.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(FrontEnd {
        ready_log: spec.get("readyLog").and_then(string_array)?,
        ready_line: text("readyLine")?,
        button: text("button")?,
    })
}

pub struct LaunchCandidate {
    pub parts: Vec<String>,
    pub args: Vec<String>,
}

pub fn launch_candidates(profile: &Value) -> Vec<LaunchCandidate> {
    if let Some(list) = profile.get("launchCandidates").and_then(Value::as_array) {
        let candidates: Vec<LaunchCandidate> = list
            .iter()
            .filter_map(|entry| {
                Some(LaunchCandidate {
                    parts: string_array(entry.get("path")?)?,
                    args: entry.get("args").and_then(string_array).unwrap_or_default(),
                })
            })
            .collect();
        if !candidates.is_empty() {
            return candidates;
        }
    }

    vec![LaunchCandidate {
        parts: vec![executable_name(profile).to_string()],
        args: Vec::new(),
    }]
}

pub fn mod_config(profile: &Value) -> Option<&Value> {
    profile.get("mods").filter(|v| v.is_object())
}

pub fn mod_variant(profile: &Value) -> Option<&str> {
    mod_config(profile)?.get("variant")?.as_str()
}

pub fn mod_loader_mode(profile: &Value) -> &str {
    mod_config(profile)
        .and_then(|m| m.get("loaderMode"))
        .and_then(Value::as_str)
        .unwrap_or("hook")
}

pub fn mod_force_dx11_args(profile: &Value) -> Vec<String> {
    mod_config(profile)
        .and_then(|m| m.get("forceDx11Args"))
        .and_then(string_array)
        .unwrap_or_default()
}

pub fn mod_gamebanana_id(profile: &Value) -> Option<u64> {
    mod_config(profile)?.get("gameBananaId")?.as_u64()
}

pub fn mod_ini_defaults(profile: &Value) -> Vec<(String, String, String)> {
    let Some(Value::Object(sections)) = mod_config(profile).and_then(|m| m.get("modIniDefaults"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (section, keys) in sections {
        let Value::Object(keys) = keys else {
            continue;
        };
        for (key, value) in keys {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Bool(b) => u8::from(*b).to_string(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            out.push((section.clone(), key.clone(), text));
        }
    }
    out
}

pub fn mod_is_experimental(profile: &Value) -> bool {
    mod_config(profile)
        .and_then(|m| m.get("experimental"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn string_array(v: &Value) -> Option<Vec<String>> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(
        arr.iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
    )
}

// ------------ Game Path Checks ------------
// Figures out where a game really lives from the folder someone picked, looking a few levels in each direction for its executable, and decides if that folder is acceptable.
// It also holds the Windows path length budget, since some games break when installed too deep.

pub struct Validation {
    pub is_valid: bool,
    pub error: Option<String>,
    pub resolved_path: Option<String>,
}

impl Validation {
    fn invalid(error: impl Into<String>) -> Self {
        Self {
            is_valid: false,
            error: Some(error.into()),
            resolved_path: None,
        }
    }
}

fn resolve(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

const WALK_DIR_BUDGET: usize = 5_000;
const WHOLE_DRIVE: &str = "Pick the game's own folder, not a whole drive.";

fn is_volume_root(path: &Path) -> bool {
    resolve(path).parent().is_none()
}

fn skipped_dir(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
    if entry
        .metadata()
        .is_ok_and(|m| m.file_attributes() & HIDDEN_OR_SYSTEM != 0)
    {
        return true;
    }
    std::env::var_os("SystemRoot")
        .is_some_and(|root| entry.path().as_os_str().eq_ignore_ascii_case(&root))
}

fn subdirs(dir: &Path, budget: &mut usize) -> Option<Vec<PathBuf>> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    Some(
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false) && !skipped_dir(e))
            .map(|e| e.path())
            .collect(),
    )
}

pub fn find_game_install_root(
    game_path: &Path,
    executable_name: &str,
    max_depth: u32,
) -> Option<PathBuf> {
    if executable_name.is_empty() {
        return None;
    }
    if game_path.join(executable_name).exists() {
        return Some(game_path.to_path_buf());
    }
    if is_volume_root(game_path) {
        return None;
    }

    fn walk(
        dir: &Path,
        exe: &str,
        depth: u32,
        max_depth: u32,
        budget: &mut usize,
    ) -> Option<PathBuf> {
        if depth > max_depth {
            return None;
        }
        let entries = subdirs(dir, budget)?;

        for sub in &entries {
            if sub.join(exe).exists() {
                return Some(sub.clone());
            }
        }
        for sub in &entries {
            if let Some(found) = walk(sub, exe, depth + 1, max_depth, budget) {
                return Some(found);
            }
        }
        None
    }

    let mut budget = WALK_DIR_BUDGET;
    walk(game_path, executable_name, 1, max_depth, &mut budget)
}

pub fn find_install_root_by_marker(
    game_path: &Path,
    marker_parts: &[String],
    max_depth: u32,
) -> Option<PathBuf> {
    if marker_parts.is_empty() {
        return None;
    }

    let marker_exists = |root: &Path| {
        let mut p = root.to_path_buf();
        for part in marker_parts {
            p.push(part);
        }
        p.exists()
    };

    let mut dir = resolve(game_path);
    for _ in 0..5 {
        if marker_exists(&dir) {
            return Some(dir);
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }

    if is_volume_root(game_path) {
        return None;
    }

    fn walk(
        dir: &Path,
        depth: u32,
        max_depth: u32,
        marker_exists: &dyn Fn(&Path) -> bool,
        budget: &mut usize,
    ) -> Option<PathBuf> {
        if depth > max_depth {
            return None;
        }
        if marker_exists(dir) {
            return Some(dir.to_path_buf());
        }
        if depth == max_depth {
            return None;
        }
        for sub in subdirs(dir, budget)? {
            if let Some(found) = walk(&sub, depth + 1, max_depth, marker_exists, budget) {
                return Some(found);
            }
        }
        None
    }

    let mut budget = WALK_DIR_BUDGET;
    walk(&resolve(game_path), 0, max_depth, &marker_exists, &mut budget)
}

pub fn resolve_launch(install_root: &Path, profile: &Value) -> (PathBuf, Vec<String>) {
    let candidates = launch_candidates(profile);
    let joined = |candidate: &LaunchCandidate| {
        candidate
            .parts
            .iter()
            .fold(install_root.to_path_buf(), |p, part| p.join(part))
    };
    for candidate in &candidates {
        let path = joined(candidate);
        if path.exists() {
            return (path, candidate.args.clone());
        }
    }
    match candidates.first() {
        Some(first) => (joined(first), first.args.clone()),
        None => (
            install_root.join(executable_name(profile)),
            Vec::new(),
        ),
    }
}

pub fn launch_executable_path(install_root: &Path, profile: &Value) -> PathBuf {
    resolve_launch(install_root, profile).0
}

pub const WINDOWS_MAX_PATH: usize = 259;

pub fn max_install_root_len(profile: &Value) -> usize {
    WINDOWS_MAX_PATH.saturating_sub(max_path_tail(profile))
}

pub fn fits_path_budget(root: &Path, profile: &Value) -> bool {
    root_len(root) <= max_install_root_len(profile)
}

pub(crate) fn root_len(root: &Path) -> usize {
    root.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .encode_utf16()
        .count()
}

pub fn path_budget_error(root: &Path, profile: &Value) -> Option<String> {
    let limit = max_install_root_len(profile);
    let len = root_len(root);
    if len <= limit {
        return None;
    }
    Some(format!(
        "This folder is {} character{} too deep for {}. Windows limits the game to {WINDOWS_MAX_PATH}-character paths, and {} needs {} of those for the files it writes itself, so its folder has to be at most {limit} characters long (this one is {len}). Pick a shorter folder, such as one directly off C:\\.",
        len - limit,
        if len - limit == 1 { "" } else { "s" },
        display_name(profile),
        display_name(profile),
        max_path_tail(profile),
    ))
}

pub fn validate_game_path_for_profile(game_path: &str, profile: &Value) -> Validation {
    if game_path.is_empty() {
        return Validation::invalid("No path provided.");
    }
    let picked = Path::new(game_path);

    if let Some(marker) = install_root_marker(profile) {
        let Some(install_root) = find_install_root_by_marker(picked, &marker, 4) else {
            return Validation::invalid(if is_volume_root(picked) {
                WHOLE_DRIVE
            } else {
                "Could not find NTE game files in this folder or its parents."
            });
        };
        let launch_exe = launch_executable_path(&install_root, profile);
        if !launch_exe.exists() {
            let base = launch_exe
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            return Validation::invalid(format!(
                "\"{base}\" not found under the detected install folder."
            ));
        }
        let selected = resolve(picked);
        let resolved = resolve(&install_root);
        return Validation {
            is_valid: true,
            error: None,
            resolved_path: (resolved != selected)
                .then(|| install_root.to_string_lossy().to_string()),
        };
    }

    let exe = executable_name(profile);
    let Some(resolved) = find_game_install_root(picked, exe, 4) else {
        return Validation::invalid(if is_volume_root(picked) {
            WHOLE_DRIVE.to_string()
        } else {
            format!("\"{exe}\" not found in this folder or its subfolders.")
        });
    };
    let resolved_str = resolved.to_string_lossy().to_string();
    Validation {
        is_valid: true,
        error: None,
        resolved_path: (resolved_str != game_path).then_some(resolved_str),
    }
}

// ------------ Default Install Folders ------------
// The places Peebify offers to install a game into, best first, and whether a game already sits in one of them.
pub(crate) const GAMES_DIR_NAME: &str = "games";

pub(crate) fn launcher_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default()
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default()
    }
}

pub(crate) fn is_protected_root(dir: &Path) -> bool {
    let protected_roots: Vec<String> = [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "SystemRoot",
        "windir",
    ]
    .iter()
    .filter_map(|var| std::env::var(var).ok())
    .filter(|v| !v.is_empty())
    .map(|v| v.to_lowercase())
    .collect();
    let dir_lower = dir.to_string_lossy().to_lowercase();
    protected_roots
        .iter()
        .any(|root| dir_lower == *root || dir_lower.starts_with(&format!("{root}\\")))
}

pub(crate) fn default_install_dirs(profile: &Value) -> Vec<(&'static str, PathBuf)> {
    let folder_name = install_folder_name(profile);

    let mut candidates: Vec<(&'static str, PathBuf)> = Vec::with_capacity(3);

    let dir = launcher_dir();
    if !is_protected_root(&dir) {
        candidates.push(("launcher", dir.join(GAMES_DIR_NAME).join(&folder_name)));
    }

    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if !home.is_empty() {
        candidates.push((
            "user-home",
            Path::new(&home).join("Peebify Games").join(&folder_name),
        ));
    }

    candidates.push(("system-drive", system_drive_games_root().join(&folder_name)));

    candidates.sort_by_key(|(_, dir)| !fits_path_budget(dir, profile));
    candidates
}

pub(crate) fn system_drive_games_root() -> PathBuf {
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    Path::new(&format!("{system_drive}\\")).join("Peebify Games")
}

pub(crate) fn install_in_default_dirs(profile: &'static Value) -> Option<String> {
    default_install_dirs(profile)
        .into_iter()
        .find_map(|(_, dir)| {
            if !dir.is_dir() {
                return None;
            }
            let picked = dir.to_string_lossy().to_string();
            let validation = validate_game_path_for_profile(&picked, profile);
            if !validation.is_valid {
                return None;
            }
            let resolved = validation.resolved_path.unwrap_or(picked);
            if is_unfinished_fresh_install(&dir, Path::new(&resolved)) {
                log::info!(
                    "Startup scan: skipped {resolved} because a fresh install there never finished."
                );
                return None;
            }
            Some(resolved)
        })
}

pub(crate) fn is_unfinished_fresh_install(dir: &Path, resolved: &Path) -> bool {
    super::validator::install_marker_owns_dir(dir) == Some(true)
        || super::validator::install_marker_owns_dir(resolved) == Some(true)
}

#[cfg(test)]
mod path_budget_tests {
    use super::*;

    const RE1999_WORST_TAIL: &str = concat!(
        r"reverse1999_Data\StreamingAssets\PersistentRoot\_tmp1\60001\Windows\",
        r"b506cfbcc5e4cd49f865962da80b7300\",
        "hotupdate_847690_49bb686f5d6b59c49f47158e0c30c633_",
        "7256a44a1d7e5767113affb11ad09e8d.zip",
    );

    #[test]
    fn the_budget_covers_the_path_the_game_actually_failed_on() {
        let profile = profile("re1999");
        let needed = RE1999_WORST_TAIL.len() + 1;
        assert!(
            needed <= max_path_tail(profile),
            "{} needs {needed} characters below its folder, but the budget is {}",
            display_name(profile),
            max_path_tail(profile),
        );
    }

    #[test]
    fn the_folder_that_broke_the_game_is_refused() {
        let profile = profile("re1999");
        let too_deep = Path::new(
            r"C:\Users\Alice\AppData\Local\Programs\Peebify Launcher\games\Reverse 1999",
        );

        assert!(!fits_path_budget(too_deep, profile));
        let message = path_budget_error(too_deep, profile).expect("a reason");
        assert!(message.contains("Reverse: 1999"), "unhelpful: {message}");

        let full = too_deep.join(RE1999_WORST_TAIL);
        assert!(full.to_string_lossy().chars().count() > WINDOWS_MAX_PATH);
    }

    #[test]
    fn a_short_folder_is_accepted_and_has_no_complaint() {
        let profile = profile("re1999");
        let fine = Path::new(r"C:\Users\Alice\Peebify Games\Reverse 1999");

        assert!(fits_path_budget(fine, profile));
        assert!(path_budget_error(fine, profile).is_none());
        assert!(
            fine.join(RE1999_WORST_TAIL)
                .to_string_lossy()
                .chars()
                .count()
                <= WINDOWS_MAX_PATH
        );
    }

    #[test]
    fn a_trailing_separator_does_not_change_the_verdict() {
        let profile = profile("re1999");
        let limit = max_install_root_len(profile);
        let exact = "a".repeat(limit);

        assert!(fits_path_budget(Path::new(&exact), profile));
        assert!(fits_path_budget(Path::new(&format!("{exact}\\")), profile));
        assert!(!fits_path_budget(Path::new(&format!("{exact}a")), profile));
    }

    #[test]
    fn characters_outside_the_bmp_count_as_two_path_units() {
        let profile = profile("re1999");
        let limit = max_install_root_len(profile);
        let ascii = "a".repeat(limit - 1);
        let with_emoji = format!("{ascii}\u{1F600}");

        assert_eq!(root_len(Path::new(&with_emoji)), limit + 1);
        assert!(!fits_path_budget(Path::new(&with_emoji), profile));
        assert!(path_budget_error(Path::new(&with_emoji), profile).is_some());
        assert!(fits_path_budget(Path::new(&format!("{ascii}\u{e9}")), profile));
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_root_is_not_searched_below() {
        assert!(is_volume_root(Path::new(r"C:\")));
        assert!(is_volume_root(Path::new(r"D:\")));
        assert!(is_volume_root(Path::new(r"\\server\share\")));
        assert!(!is_volume_root(Path::new(r"C:\Games")));
        assert!(!is_volume_root(Path::new(r"D:\Games\Wuthering Waves")));

        assert_eq!(find_game_install_root(Path::new(r"C:\"), "no-such-game.exe", 4), None);
        let validation =
            validate_game_path_for_profile(r"C:\", profile("re1999"));
        assert!(!validation.is_valid);
        assert_eq!(validation.error.as_deref(), Some(WHOLE_DRIVE));
    }

    #[test]
    fn a_game_below_the_picked_folder_is_still_found() {
        let root = std::env::temp_dir().join(format!("peebify-locate-{}", uuid::Uuid::new_v4()));
        let game = root.join("Games").join("Game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("game.exe"), b"x").unwrap();

        let found = find_game_install_root(&root, "game.exe", 4);
        let direct = find_game_install_root(&game, "game.exe", 4);
        let too_shallow = find_game_install_root(&root, "game.exe", 1);
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(found, Some(game.clone()));
        assert_eq!(direct, Some(game));
        assert_eq!(too_shallow, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_names_a_known_install_mode() {
        for id in GAME_IDS {
            assert_ne!(InstallMode::of(profile(id)), InstallMode::Unknown, "{id}");
        }
        assert_eq!(InstallMode::of(&json!({})), InstallMode::Default);
        assert_eq!(InstallMode::of(&json!({ "installMode": "hoyoplay" })), InstallMode::Unknown);
    }

    #[test]
    fn only_the_nte_launcher_is_driven_out_of_sight() {
        let nte = profile("nte");
        let driven = front_end(nte, std::path::Path::new(r"D:\Games\NTE\NTEGlobal\ntegloBALgame.EXE"))
            .expect("NTE's launcher is driven");
        assert_eq!(driven.ready_log.last().map(String::as_str), Some("NTEGlobalGame.log"));
        assert!(!driven.ready_line.is_empty() && !driven.button.is_empty());
        assert!(front_end(
            nte,
            std::path::Path::new(r"D:\Games\NTE\Client\WindowsNoEditor\HT\Binaries\Win64\HTGame.exe")
        )
        .is_none());
        assert!(front_end(profile("wuwa"), std::path::Path::new(r"D:\Games\NTEGlobalGame.exe")).is_none());
    }

    fn dir_for<'a>(dirs: &'a [(&'static str, PathBuf)], kind: &str) -> Option<&'a std::path::Path> {
        dirs.iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, dir)| dir.as_path())
    }

    #[test]
    fn a_game_folder_is_offered_next_to_the_launcher_and_under_the_profile() {
        let dirs = default_install_dirs(profile("wuwa"));

        assert_eq!(dirs.len(), 3, "tests do not run from a protected root");
        assert!(dir_for(&dirs, "launcher")
            .expect("a launcher folder")
            .ends_with(Path::new("games").join("Wuthering Waves")));
        assert!(dir_for(&dirs, "user-home")
            .expect("a profile folder")
            .ends_with(Path::new("Peebify Games").join("Wuthering Waves")));
        assert!(dir_for(&dirs, "system-drive")
            .expect("a drive-root folder")
            .ends_with(Path::new("Peebify Games").join("Wuthering Waves")));
    }

    #[test]
    fn a_name_windows_cannot_use_is_stripped_the_same_way_on_both_sides() {
        let dirs = default_install_dirs(profile("gf2"));

        let launcher = dir_for(&dirs, "launcher").expect("a launcher folder");
        assert!(
            launcher.ends_with(Path::new("games").join("Girls' Frontline 2 Exilium")),
            "unexpected folder: {}",
            launcher.display()
        );
    }

    #[test]
    fn a_game_that_needs_room_is_not_offered_the_deepest_folder_first() {
        let profile = profile("re1999");
        let dirs = default_install_dirs(profile);

        let launcher = dir_for(&dirs, "launcher").expect("a launcher folder");
        if !fits_path_budget(launcher, profile) {
            assert_ne!(
                dirs[0].0,
                "launcher",
                "offered a folder the game cannot write from: {}",
                launcher.display()
            );
        }
        assert!(
            fits_path_budget(dirs[0].1.as_path(), profile),
            "the first choice has no room for the game's own paths: {}",
            dirs[0].1.display()
        );
        assert!(
            dirs.iter().any(|(kind, _)| *kind == "launcher"),
            "detection can no longer find installs next to the launcher"
        );
    }

    #[test]
    fn nothing_is_adopted_from_a_folder_that_holds_no_game() {
        let empty = std::env::temp_dir().join(format!(
            "peebify-detect-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&empty).unwrap();

        let validation = validate_game_path_for_profile(
            &empty.to_string_lossy(),
            profile("wuwa"),
        );

        let _ = std::fs::remove_dir_all(&empty);
        assert!(!validation.is_valid);
    }

    #[test]
    fn only_a_fresh_install_that_never_finished_blocks_adoption() {
        let tmp = crate::backend::fs_util::TempDir::new("marker");
        let dir = tmp.path().to_path_buf();
        let nested = dir.join("Game");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(!is_unfinished_fresh_install(&dir, &nested));

        std::fs::write(dir.join(".peebify-install"), r#"{"ownedDir": false}"#).unwrap();
        assert!(!is_unfinished_fresh_install(&dir, &nested));

        std::fs::write(dir.join(".peebify-install"), r#"{"ownedDir": true}"#).unwrap();
        assert!(is_unfinished_fresh_install(&dir, &nested));

        std::fs::remove_file(dir.join(".peebify-install")).unwrap();
        std::fs::write(nested.join(".peebify-install"), r#"{"ownedDir": true}"#).unwrap();
        assert!(is_unfinished_fresh_install(&dir, &nested));

    }

    #[test]
    fn the_games_folder_matches_the_one_the_installer_spares() {
        let installer = include_str!("../../../installer/src/consts.rs");
        let declared = format!("pub const GAMES_DIR_NAME: &str = \"{GAMES_DIR_NAME}\";");
        assert!(installer.contains(&declared));
    }
}
