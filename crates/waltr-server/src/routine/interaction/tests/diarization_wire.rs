use super::*;

#[tokio::test]
async fn production_diarization_sends_opt_in_and_final_replaces_cumulative_interims() {
    let server = fixtures::SpeechServer::start().await;
    let mut request = fixtures::request_with_diarization(TRANSCRIBE, true).await;
    let configuration = Arc::make_mut(&mut request.configuration);
    assert!(
        configuration.providers["speech"].models["dictation"]
            .recognition
            .as_ref()
            .unwrap()
            .diarization
    );
    configuration.providers.get_mut("speech").unwrap().url = server.endpoint.clone();
    let (host, _sandbox) = fixtures::Host::new(Arc::new(tokio::sync::Notify::new())).await;
    let mut interaction = start_sessions(request, host).unwrap();
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Opened { .. }
    ));
    let mut state = State::new(RoutineStreamFormat::Text, 4096);
    for _ in 0..2 {
        interaction
            .inputs
            .get_mut("audio")
            .unwrap()
            .send(Chunk::Audio(vec![0; 4]))
            .await
            .unwrap();
        let Event::Chunk {
            chunk: Chunk::Text(text),
            ..
        } = next(&mut interaction.events).await
        else {
            panic!("expected interim text");
        };
        assert!(!text.committed);
        assert_eq!(text.text, "Hello, there!");
        state.accept(&Chunk::Text(text)).unwrap();
    }
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    let Event::Chunk {
        chunk: Chunk::Text(text),
        ..
    } = next(&mut interaction.events).await
    else {
        panic!("expected final speaker text");
    };
    assert!(text.committed && text.replace);
    state.accept(&Chunk::Text(text)).unwrap();
    state.close().unwrap();
    assert_eq!(state.collect(" ", 4096).unwrap(), "S1: Hello,\nS2: there!");
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Closed { error: None, .. }
    ));
    assert!(matches!(
        next(&mut interaction.events).await,
        Event::Completed(Ok(_))
    ));
    interaction.join().await;
}
