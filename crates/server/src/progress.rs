//! LSP work-done progress reporting.
//!
//! Callers own the user-visible operation lifecycle; this module hides the
//! protocol handshake and notification details. A disabled reporter is a
//! no-op, so callers can report phases without branching on client support or
//! runtime settings.

use tower_lsp::lsp_types::{
    notification, request, NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkDoneProgressReport,
};
use tower_lsp::Client;

pub(crate) struct ProgressReporter {
    client: Client,
    token: Option<NumberOrString>,
}

impl ProgressReporter {
    pub(crate) async fn begin(
        client: &Client,
        enabled: bool,
        token: NumberOrString,
        title: &str,
        message: &str,
    ) -> Self {
        let mut reporter = Self {
            client: client.clone(),
            token: None,
        };
        if !enabled {
            return reporter;
        }
        if client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .is_err()
        {
            return reporter;
        }
        client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: title.into(),
                        message: Some(message.into()),
                        cancellable: Some(false),
                        percentage: None,
                    },
                )),
            })
            .await;
        reporter.token = Some(token);
        reporter
    }

    pub(crate) async fn report(&self, message: impl Into<String>, percentage: Option<u32>) {
        let Some(token) = &self.token else { return };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                    WorkDoneProgressReport {
                        message: Some(message.into()),
                        percentage: percentage.map(|value| value.min(100)),
                        cancellable: Some(false),
                    },
                )),
            })
            .await;
    }

    pub(crate) async fn end(self, message: impl Into<String>) {
        let Some(token) = self.token else { return };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(message.into()),
                })),
            })
            .await;
    }
}
