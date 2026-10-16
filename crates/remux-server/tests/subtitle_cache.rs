use axum_test::TestServer;
use chrono::Utc;
use http::{HeaderValue, header::AUTHORIZATION};
use remux_sdks::remux::{
    MediaSourceInfo, MediaSourceRemuxInfo, MediaStream, MediaStreamType, ProbeOrigin,
};
use remux_server::{
    Config, db, init_app_with_ctx,
    stream::{StreamDescriptor, StreamInfo},
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn cold_item_autoplay_alias_only_advertises_ready_cached_dub_text() {
    let provider = httpmock::MockServer::start_async().await;
    provider
        .mock_async(|when, then| {
            when.path("/manifest.json");
            then.status(200)
                .json_body(json!({
                    "id":"fixture", "version":"1.0.0", "name":"fixture",
                    "resources":["stream"], "types":["movie"], "catalogs":[],
                }));
        })
        .await;
    provider
        .mock_async(|when, then| {
            when.method(httpmock::Method::HEAD)
                .path_matches(regex::Regex::new(r"^/mux/").unwrap());
            then.status(200)
                .header("Content-Length", "10000");
        })
        .await;
    provider
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path_matches(
                    regex::Regex::new(r"^/subtitles/[^/]+/23\.srt$").unwrap(),
                );
            then.status(200)
                .body("1\n00:00:01,000 --> 00:00:02,000\nXin chao\n\n");
        })
        .await;
    let data = tempfile::tempdir().unwrap();
    let (app, ctx) = init_app_with_ctx(Config {
        data_dir: data
            .path()
            .to_path_buf(),
        database_url: Some("sqlite::memory:".into()),
        torrent_http_port: None,
        torrent_peer_port: None,
        disable_dht: true,
        dubmux_subtitle_cache_url: Some(provider.base_url()),
        ..Default::default()
    })
    .await
    .unwrap();
    let server = TestServer::builder()
        .mock_transport()
        .expect_success_by_default()
        .build(app)
        .unwrap();
    server
        .post("/startup/user")
        .json(&json!({"Name":"test","Password":"test"}))
        .await;
    server
        .post("/startup/complete")
        .await;
    let auth_base = "MediaBrowser Client=\"Test\", Device=\"Test\", DeviceId=\"test-device\", Version=\"1.0.0\"";
    let authenticated: serde_json::Value = server
        .post("/users/authenticatebyname")
        .add_header(AUTHORIZATION, HeaderValue::from_static(auth_base))
        .json(&json!({"Username":"test","Pw":"test"}))
        .await
        .json();
    let token = authenticated["AccessToken"]
        .as_str()
        .unwrap();
    let auth =
        HeaderValue::from_str(&format!("{auth_base}, Token=\"{token}\"")).unwrap();
    let now = Utc::now().naive_utc();
    let created: serde_json::Value = server.post("/addons").add_header(AUTHORIZATION, auth.clone())
        .json(&json!({"name":"cached-subtitle-fixture", "resources":["stream"],
            "preset":{"kind":"stremio","config":{"manifest_url":provider.url("/manifest.json")}}}))
        .await.json();
    let addon_id = Uuid::parse_str(
        created["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    server
        .post(&format!("/addons/{addon_id}"))
        .add_header(AUTHORIZATION, auth.clone())
        .json(&json!({"httpRedirectStream":true,"subtitleExtraction":true}))
        .await;
    let external_ids = db::ExternalIds {
        imdb: db::NonEmptyString::try_new(format!("tt-cold-cache-{}", Uuid::new_v4()))
            .ok(),
        ..Default::default()
    };
    let mut movie = db::Media {
        id: Uuid::from(&db::MediaIdRaw {
            kind: db::MediaKind::Movie,
            external_ids: external_ids.clone(),
            season: None,
            episode: None,
        }),
        title: "Cold cached dub".into(),
        kind: db::MediaKind::Movie,
        external_ids,
        runtime: Some(10),
        streams_refreshed_at: Some(now),
        created_at: now,
        updated_at: now,
        ..Default::default()
    };
    movie
        .save(&ctx.db)
        .await
        .unwrap();
    let mut row = db::Media {
        id: Uuid::new_v4(),
        title: "Cached dub source".into(),
        kind: db::MediaKind::Stream,
        parent_id: Some(movie.id),
        stream_info: Some(StreamInfo {
            descriptor: StreamDescriptor::http(format!(
                "{}/mux/{}/{}/file.ts?video=offline",
                provider.base_url(),
                Uuid::new_v4(),
                Uuid::new_v4()
            )),
            addon_id: Some(addon_id),
            filename: Some("release.VNDub-kkphim.ts".into()),
            size: Some(10000),
            ..Default::default()
        }),
        probe_data: Some(MediaSourceInfo {
            container: Some(
                "ts".parse()
                    .unwrap(),
            ),
            run_time_ticks: Some(100_000_000),
            bitrate: Some(8_000_000),
            remux: Some(MediaSourceRemuxInfo {
                source: Some(ProbeOrigin::Ffprobe),
                ..Default::default()
            }),
            media_streams: vec![
                MediaStream {
                    index: 0,
                    type_: Some(MediaStreamType::Video),
                    codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    ..Default::default()
                },
                MediaStream {
                    index: 1,
                    type_: Some(MediaStreamType::Audio),
                    codec: Some("aac".into()),
                    ..Default::default()
                },
                MediaStream {
                    index: 123,
                    type_: Some(MediaStreamType::Subtitle),
                    codec: Some("subrip".into()),
                    language: Some("vie".into()),
                    ..Default::default()
                },
                MediaStream {
                    index: 124,
                    type_: Some(MediaStreamType::Subtitle),
                    codec: Some("subrip".into()),
                    language: Some("vie".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        created_at: now,
        updated_at: now,
        ..Default::default()
    };
    row.save(&ctx.db)
        .await
        .unwrap();
    // Media::save does not write the stream-refresh timestamp. Keep the stored
    // stream list fresh so this test never asks a provider for replacement rows.
    sqlx::query("UPDATE media SET streams_refreshed_at=? WHERE id=?")
        .bind(now)
        .bind(movie.id)
        .execute(&ctx.db)
        .await
        .unwrap();
    db::Settings::set_config(
        &ctx.db,
        &remux_sdks::remux::ServerConfiguration {
            enable_subtitles_detail: Some(true),
            subtitle_languages: Some(vec!["vi".into()]),
            stream_groups_show_ungrouped: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let response = server
        .get(&format!("/items/{}", movie.id))
        .add_header(AUTHORIZATION, auth.clone())
        .await;
    let body: serde_json::Value = response.json();
    let streams = body["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap();
    let summary: Vec<_> = streams
        .iter()
        .map(
            |s| json!({"index":s["Index"],"type":s["Type"],"external":s["IsExternal"]}),
        )
        .collect();
    assert!(
        streams
            .iter()
            .any(|s| s["Index"] == 123
                && s["Type"] == "Subtitle"
                && s["IsExternal"] == true),
        "{summary:?}"
    );
    assert!(
        !streams
            .iter()
            .any(|s| s["Index"] == 124),
        "uncached mux text must stay hidden: {summary:?}"
    );
    let subtitles = server
        .get(&format!(
            "/videos/{0}/{0}/subtitles/123/stream.webvtt",
            movie.id
        ))
        .add_header(AUTHORIZATION, auth)
        .await;
    assert!(
        subtitles
            .text()
            .contains("Xin chao")
    );
    ctx.shutdown()
        .await;
}
