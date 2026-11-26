//! Remembering the model you chose, and the config file's servers. Its own
//! test binary: it points XDG_STATE_HOME and XDG_CONFIG_HOME at a temporary
//! folder for the whole process, so your real choices are never touched.

mod common;

use common::*;
use mathbuddy::{
    config::{self, State},
    core::{Core, Settings},
};

fn settings(ollama: &str, llamacpp: &str) -> Settings {
    Settings {
        host: Some(ollama.into()),
        llamacpp: vec![llamacpp.into()],
        ctx: 8192,
        hints: true,
        ..Default::default()
    }
}

fn notes(core: &mut Core) -> String {
    core.notes.drain(..).collect::<Vec<_>>().join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn remembers_the_model_and_never_swaps_one_in() {
    let tmp = TempDir::new("choices");
    // SAFETY: set once, before anything else in this process reads them
    unsafe {
        std::env::set_var("XDG_STATE_HOME", tmp.0.join("state"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.0.join("config"));
    }
    let ollama = FakeOllama::start(&["qwen3.5:latest"], vec![]);
    let llama = FakeOllama::start(&["bonsai-27b", "other-model"], vec![]);

    // first run: nothing remembered, the best Ollama model
    let (mut core, _rx) = Core::new(settings(&ollama.host, &llama.host));
    core.choose_model().await;
    assert_eq!(
        core.models.label(),
        "qwen3.5:latest",
        "{}",
        notes(&mut core)
    );
    assert!(
        State::load().model.is_none(),
        "an automatic pick isn't a choice"
    );

    // you pick Bonsai on llama.cpp: next time it's back
    core.switch_model(1, "bonsai-27b");
    let (mut core, _rx) = Core::new(settings(&ollama.host, &llama.host));
    core.choose_model().await;
    assert_eq!(core.models.label(), "bonsai-27b (llama.cpp)");
    assert!(notes(&mut core).contains("as last time"));

    // a remembered model that isn't loaded while another is: not loaded
    // behind your back, Ollama's best instead
    core.switch_model(1, "other-model");
    let (mut core, _rx) = Core::new(settings(&ollama.host, &llama.host));
    core.choose_model().await;
    assert_eq!(core.models.label(), "qwen3.5:latest");
    assert!(notes(&mut core).contains("could unload another model"));

    // no Ollama: the model llama.cpp already has loaded
    let (mut core, _rx) = Core::new(settings("127.0.0.1:9", &llama.host));
    core.choose_model().await;
    assert_eq!(
        core.models.label(),
        "bonsai-27b (llama.cpp)",
        "{}",
        notes(&mut core)
    );

    // /llamacpp ADDRESS adds a server and saves it in the config file
    let (mut core, _rx) = Core::new(Settings {
        llamacpp: vec![],
        ..settings(&ollama.host, "")
    });
    core.add_llamacpp(&llama.host).await;
    assert_eq!(core.models.servers.len(), 2);
    let (cfg, problem) = config::Config::load();
    assert_eq!(problem, None);
    assert_eq!(cfg.llamacpp, vec![llama.host.clone()]);
}
