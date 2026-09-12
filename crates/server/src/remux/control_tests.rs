use super::tests::{job_spec, temp_dir, test_app, wait_for_terminal_cleanup};
use super::*;

fn control(app: &App, method: &str, query: &str) -> serde_json::Value {
    let request = rusty_dlna_http::HttpRequest::parse_headers(&format!(
        "{method} /api/web/transcode/42?{query} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
    ))
    .unwrap();
    let response = crate::web_ui::transcode_status(app, &request);
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    serde_json::from_slice(&response.body).unwrap()
}

#[test]
fn vanished_source_controls_cover_preparation_playback_seek_and_disconnect() {
    for rename in [false, true] {
        for phase in ["preparation", "playback", "seek", "disconnect"] {
            let dir = temp_dir(&format!("vanished-{rename}-{phase}"));
            let app = test_app(&dir, 2);
            let mut spec = job_spec(&dir, "vanished", vec!["sleep".into(), "30".into()]);
            spec.job_key = "web:42:vanished".into();
            spec.web_session_id = Some(9);
            spec.web_request_id = Some(77);
            spec.cacheable = false;
            spec.continue_after_disconnect = false;
            spec.source_file = Some(Arc::new(std::fs::File::open(&spec.src).unwrap()));
            let growing = phase == "playback" || phase == "disconnect";
            let command = if growing {
                format!(
                    "dd if=/dev/zero of=\"$1\" bs={FIRST_BYTES} count=1 2>/dev/null; exec sleep 30"
                )
            } else {
                "exec sleep 30".into()
            };
            // This supervised producer holds the real admitted descriptor and
            // ignores the media-input arguments after its output-path argument.
            spec.args = vec![
                "sh".into(),
                "-c".into(),
                command.into(),
                "control-fixture".into(),
                cache_part(&spec.dest).into_os_string(),
                "-i".into(),
                "/proc/self/fd/3".into(),
            ];
            let source = spec.src.clone();
            let job = attach_for_client(app.clone(), spec.clone()).unwrap();
            if growing {
                let deadline = Instant::now() + Duration::from_secs(10);
                while job.state() != RemuxState::Growing {
                    assert!(
                        Instant::now() < deadline,
                        "producer state: {:?}",
                        job.state()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            if rename {
                std::fs::rename(&source, dir.join("renamed.mkv")).unwrap();
            } else {
                std::fs::remove_file(&source).unwrap();
            }
            // No catalog entry is needed by a descriptor-owning generation.
            assert!(crate::read_recover(&app.catalog)
                .get_item_by_detail(42)
                .is_none());
            let job = if phase == "seek" {
                let mut seek = spec.clone();
                seek.web_request_id = Some(78);
                let replacement = attach_for_client(app.clone(), seek).unwrap();
                assert!(Arc::ptr_eq(&job, &replacement));
                replacement
            } else {
                job
            };
            let request = if phase == "seek" { 78 } else { 77 };
            if phase == "disconnect" {
                job.detach_client(
                    app.clone(),
                    spec.job_key.clone(),
                    false,
                    Duration::from_secs(30),
                    Duration::ZERO,
                );
            }
            let query = format!("session=9&request={request}");
            let status = control(&app, "GET", &query);
            assert!(matches!(
                status["state"].as_str(),
                Some("starting" | "producing" | "preprocessing")
            ));
            control(&app, "POST", &format!("{query}&event=playing"));
            assert!(job.startup_observations.playing.load(Ordering::Acquire));
            if phase == "seek" {
                control(&app, "DELETE", "session=9&request=77");
                assert!(!job.cancelled.load(Ordering::Acquire));
            }
            assert_eq!(control(&app, "DELETE", &query)["state"], "cancelled");
            assert!(job.cancelled.load(Ordering::Acquire));
            assert_eq!(control(&app, "DELETE", &query)["state"], "cancelled");
            wait_for_terminal_cleanup(&app, &job);

            let request = rusty_dlna_http::HttpRequest::parse_headers(
                "GET /web/media/42.mp4?mode=compatible HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            )
            .unwrap();
            let admission =
                crate::web_ui::media(&app, &request, "127.0.0.1:12345".parse().unwrap());
            assert_eq!(admission.status, 404);
        }
    }
}

#[test]
fn vanished_source_delete_preserves_other_viewer_and_rejects_wrong_owner() {
    let dir = temp_dir("vanished-shared");
    let app = test_app(&dir, 1);
    let mut spec = job_spec(&dir, "shared", vec!["sleep".into(), "30".into()]);
    spec.job_key = "web:42:shared".into();
    spec.web_session_id = Some(9);
    spec.web_request_id = Some(77);
    spec.cacheable = false;
    let job = attach_for_client(app.clone(), spec.clone()).unwrap();
    spec.web_session_id = Some(10);
    spec.web_request_id = Some(88);
    assert!(Arc::ptr_eq(
        &job,
        &attach_for_client(app.clone(), spec.clone()).unwrap()
    ));
    std::fs::remove_file(&spec.src).unwrap();
    control(&app, "DELETE", "session=11&request=77");
    assert!(job.owns_web_request(Some(9), 77));
    control(&app, "DELETE", "session=9&request=77");
    assert!(!job.cancelled.load(Ordering::Acquire));
    assert!(job.owns_web_request(Some(10), 88));
    control(&app, "DELETE", "session=9&request=77");
    assert!(!job.cancelled.load(Ordering::Acquire));
    control(&app, "DELETE", "session=10&request=88");
    assert!(job.cancelled.load(Ordering::Acquire));
    wait_for_terminal_cleanup(&app, &job);
}
