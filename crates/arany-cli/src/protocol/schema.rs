use super::{EventLine, methods::*, rpc::Message};
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Value, json};

pub fn protocol_schema() -> Value {
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = false;
    let mut generator = settings.into_generator();
    let message = generator.subschema_for::<Message>();
    generator.subschema_for::<EventLine>();
    let methods = json!({
        "initialize": method::<InitializeParams, InitializeResult>(&mut generator),
        "thread/start": method::<ThreadStartParams, ThreadResult>(&mut generator),
        "thread/resume": method::<ThreadResumeParams, ThreadResumeResult>(&mut generator),
        "thread/settings/update": method::<ThreadSettingsUpdateParams, ThreadSettingsUpdateResult>(&mut generator),
        "thread/fork": method::<ThreadForkParams, ThreadResult>(&mut generator),
        "events/subscribe": method::<EventsSubscribeParams, EventsSubscribeResult>(&mut generator),
        "receipt/get": method::<ReceiptGetParams, ReceiptGetResult>(&mut generator),
        "turn/start": method::<TurnStartParams, TurnResult>(&mut generator),
        "turn/interrupt": method::<TurnInterruptParams, Empty>(&mut generator),
        "turn/steer": method::<TurnSteerParams, TurnSteerResult>(&mut generator),
        "goal/pause": method::<GoalPauseParams, Empty>(&mut generator),
        "goal/resume": method::<GoalResumeParams, TurnResult>(&mut generator),
    });
    let notifications = json!({
        "initialized": {"params": generator.subschema_for::<Empty>()},
        "thread/started": {"params": generator.subschema_for::<ThreadResult>()},
        "turn/started": {"params": generator.subschema_for::<TurnNotification>()},
        "turn/completed": {"params": generator.subschema_for::<TurnNotification>()},
        "item/started": {"params": generator.subschema_for::<ItemStartedNotification>()},
        "item/completed": {"params": generator.subschema_for::<ItemCompletedNotification>()},
        "events/resync": {"params": generator.subschema_for::<EventsResyncNotification>()},
        "events/committed": {"params": generator.subschema_for::<EventsCommittedNotification>()},
    });
    let approval = method::<ApprovalParams, ApprovalResult>(&mut generator);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": message.as_value()["$ref"],
        "title": "arany-protocol",
        "version": "1",
        "$defs": generator.take_definitions(true),
        "methods": methods,
        "notifications": notifications,
        "serverRequests": {
            "item/commandExecution/requestApproval": approval,
            "item/fileChange/requestApproval": approval,
            "item/tool/requestApproval": approval,
        },
    })
}

fn method<P: JsonSchema, R: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
    json!({
        "params": generator.subschema_for::<P>(),
        "result": generator.subschema_for::<R>(),
    })
}
