//! Messages reach a module in a test the way the dispatcher delivers them.

use std::sync::{Arc, Mutex, PoisonError};

use closure_sdk::testing::{Harness, Message};
use closure_sdk::{Event, EventKind, Flow, Intents, ModuleBuilder, ModuleError, Subscription};

#[tokio::test]
async fn a_module_sees_the_content_attachments_and_mentions_of_a_message() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&seen);
    let module = ModuleBuilder::new("watch", "Watching", "0.1.0")
        .unwrap()
        .intents(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
        .on(
            Subscription::new(EventKind::MessageCreated).ignore_bots(),
            move |_ctx, event| {
                let sink = Arc::clone(&sink);
                async move {
                    if let Event::MessageCreated(message) = event {
                        sink.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(format!(
                                "{}|{}|{}|{}",
                                message.content,
                                message.attachments.len(),
                                message.mentioned_users.len(),
                                message.mentions_everyone
                            ));
                    }
                    Ok::<Flow, ModuleError>(Flow::Continue)
                }
            },
        );
    let harness = Harness::new(module).unwrap();

    let flow = harness
        .message(
            Message::new(7, 3)
                .content("hi")
                .mention(9)
                .mention_everyone()
                .attachment("a.png", 10),
        )
        .await
        .unwrap();
    assert_eq!(flow, Some(Flow::Continue));
    assert_eq!(
        *seen.lock().unwrap_or_else(PoisonError::into_inner),
        ["hi|1|1|true"]
    );

    // The subscription leaves bots out, so the module does not see this one.
    let flow = harness
        .message(Message::new(8, 3).bot().content("beep"))
        .await
        .unwrap();
    assert_eq!(flow, None);
    assert_eq!(seen.lock().unwrap_or_else(PoisonError::into_inner).len(), 1);
}
