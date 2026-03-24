use std::sync::Arc;

use flume::Receiver;
use tracing::info;

use crate::app::{App, AppAction};

use super::deferred_sender::{MessageSender, SingleOrMany};

#[derive(Debug, Clone, Hash, Eq, PartialEq, Default, uniffi::Enum)]
pub enum OnboardingStep {
    #[default]
    Terms,
    CloudCheck,
    RestoreOffer,
    Restoring,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum OnboardingAction {
    AcceptTerms,
    CloudCheckComplete { has_backup: bool },
    SkipRestore,
    StartRestore,
    RestoreComplete,
    RestoreFailed { error: String },
}

type Message = OnboardingReconcileMessage;

#[derive(Debug, Clone, uniffi::Enum)]
pub enum OnboardingReconcileMessage {
    StepChanged(OnboardingStep),
    Complete,
    RestoreError(String),
}

#[uniffi::export(callback_interface)]
pub trait OnboardingManagerReconciler: Send + Sync + std::fmt::Debug + 'static {
    fn reconcile(&self, message: OnboardingReconcileMessage);
}

#[derive(Clone, Debug, uniffi::Object)]
pub struct RustOnboardingManager {
    reconciler: MessageSender<Message>,
    reconcile_receiver: Arc<Receiver<SingleOrMany<Message>>>,
}

#[uniffi::export]
impl RustOnboardingManager {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = flume::bounded(100);
        Arc::new(Self {
            reconciler: MessageSender::new(sender),
            reconcile_receiver: Arc::new(receiver),
        })
    }

    pub fn listen_for_updates(&self, reconciler: Box<dyn OnboardingManagerReconciler>) {
        let reconcile_receiver = self.reconcile_receiver.clone();

        std::thread::spawn(move || {
            while let Ok(field) = reconcile_receiver.recv() {
                match field {
                    SingleOrMany::Single(message) => reconciler.reconcile(message),
                    SingleOrMany::Many(messages) => {
                        for message in messages {
                            reconciler.reconcile(message);
                        }
                    }
                }
            }
        });
    }

    pub fn dispatch(&self, action: OnboardingAction) {
        info!("Onboarding: dispatch action={action:?}");
        match action {
            OnboardingAction::AcceptTerms => {
                App::global().handle_action(AppAction::AcceptTerms);
                self.send(Message::StepChanged(OnboardingStep::CloudCheck));
            }
            OnboardingAction::CloudCheckComplete { has_backup } => {
                if has_backup {
                    self.send(Message::StepChanged(OnboardingStep::RestoreOffer));
                } else {
                    self.send(Message::Complete);
                }
            }
            OnboardingAction::SkipRestore => {
                self.send(Message::Complete);
            }
            OnboardingAction::StartRestore => {
                self.send(Message::StepChanged(OnboardingStep::Restoring));
            }
            OnboardingAction::RestoreComplete => {
                self.send(Message::Complete);
            }
            OnboardingAction::RestoreFailed { error } => {
                self.send(Message::RestoreError(error));
            }
        }
    }
}

impl RustOnboardingManager {
    fn send(&self, message: Message) {
        self.reconciler.send(message);
    }
}
