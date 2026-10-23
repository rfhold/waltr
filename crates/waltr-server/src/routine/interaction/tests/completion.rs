use super::*;

struct CompletionAdapter {
    failure: Option<&'static str>,
    calls: std::sync::Mutex<Vec<String>>,
}
impl Adapters for CompletionAdapter {
    fn execute<'a>(&'a self, task: &'a RoutineTask, _: &'a mut TaskContext) -> AdapterFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(task.name.clone());
            if task.name == "middle"
                && let Some(code) = self.failure
            {
                return Err(Error::new(code, "scripted failure"));
            }
            Ok(TaskResult::default())
        })
    }
}

#[tokio::test]
async fn skipped_completion_enables_successors_but_failed_and_interrupted_tasks_do_not() {
    for failure in [None, Some("provider"), Some("task_interrupted")] {
        let mut source: Value = serde_yaml_ng::from_str(CLEANUP).unwrap();
        source["spec"]["outputs"][0]["streamFrom"] = serde_json::json!("$(tasks.after.output)");
        source["spec"]["results"] = serde_json::json!([]);
        source["spec"]["params"] =
            serde_json::json!([{"name": "enabled", "type": "boolean", "default": false}]);
        source["spec"]["triggers"][0]["interaction"]["inputs"] =
            serde_json::json!([{"name": "enabled"}]);
        let mut helper = source["spec"]["tasks"][2].clone();
        helper["taskSpec"]["sessionTemplate"]["userPrompt"]["text"] =
            serde_json::json!("fixed prompt");
        helper.as_object_mut().unwrap().remove("workGroup");
        let mut before = helper.clone();
        before["name"] = serde_json::json!("before");
        before["runAfter"] = serde_json::json!([]);
        before["when"] =
            serde_json::json!([{"input": "$(params.enabled)", "operator": "In", "values": [true]}]);
        let mut middle = helper.clone();
        middle["name"] = serde_json::json!("middle");
        middle["runAfter"] = serde_json::json!(["before"]);
        helper["name"] = serde_json::json!("after");
        helper["runAfter"] = serde_json::json!(["middle"]);
        source["spec"]["tasks"] = serde_json::json!([before, middle, helper]);
        let mut request = request(&serde_yaml_ng::to_string(&source).unwrap()).await;
        request.params.insert("enabled".into(), Value::Bool(false));
        let adapter = Arc::new(CompletionAdapter {
            failure,
            calls: Default::default(),
        });
        let mut interaction = start(request, adapter.clone()).unwrap();
        interaction
            .inputs
            .get_mut("audio")
            .unwrap()
            .close()
            .await
            .unwrap();
        loop {
            if let Event::Completed(result) = next(&mut interaction.events).await {
                assert_eq!(result.is_ok(), failure.is_none(), "{result:?}");
                break;
            }
        }
        interaction.join().await;
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            if failure.is_none() {
                vec!["middle", "after"]
            } else {
                vec!["middle"]
            }
        );
    }
}
