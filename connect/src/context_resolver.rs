use crate::{
    core::{Error, Session},
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context,
        transfer_state::TransferState,
    },
    state::{
        ConnectState,
        context::{ContextType, PageRef},
    },
};
use std::{
    cmp::PartialEq,
    collections::{HashMap, VecDeque},
    fmt::{Display, Formatter},
    hash::Hash,
    time::Duration,
};
use thiserror::Error as ThisError;
use tokio::time::Instant;

/// Metadata key under which a DJ context names the url its tracks come from.
const LEXICON_CONTEXT_URL: &str = "lexicon_context_url";

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Resolve {
    Uri(String),
    Context(Context),
}

/// The `hm://` url a context names for resolving its own tracks, if it has one.
///
/// A DJ context arrives with no tracks at all; its real, session-scoped track list lives behind
/// this url rather than behind `/context-resolve/v1/<uri>`.
pub(super) fn lexicon_url(context: &Context) -> Option<&str> {
    let is_hm = |url: &&String| url.starts_with("hm://");

    context
        .url
        .as_ref()
        .filter(is_hm)
        .or_else(|| context.metadata.get(LEXICON_CONTEXT_URL).filter(is_hm))
        .map(String::as_str)
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) enum ContextAction {
    Append,
    Replace,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct ResolveContext {
    resolve: Resolve,
    /// the `hm://` url to fetch instead of resolving [`Self::resolve_uri`], when the context or
    /// page names one
    resolve_url: Option<String>,
    fallback: Option<String>,
    update: ContextType,
    action: ContextAction,
}

impl ResolveContext {
    fn append_context(context_uri: &str, page: PageRef) -> Self {
        let (uri, resolve_url) = match page {
            PageRef::Uri(uri) => (uri, None),
            PageRef::Url(url) => (context_uri.to_string(), Some(url)),
        };

        Self {
            resolve: Resolve::Uri(uri),
            resolve_url,
            fallback: None,
            update: ContextType::Default,
            action: ContextAction::Append,
        }
    }

    pub fn from_uri(
        uri: impl Into<String>,
        fallback: impl Into<String>,
        update: ContextType,
        action: ContextAction,
    ) -> Self {
        let fallback_uri = fallback.into();
        Self {
            resolve: Resolve::Uri(uri.into()),
            resolve_url: None,
            fallback: (!fallback_uri.is_empty()).then_some(fallback_uri),
            update,
            action,
        }
    }

    pub fn from_uri_with_url(
        uri: impl Into<String>,
        fallback: impl Into<String>,
        resolve_url: Option<String>,
        update: ContextType,
        action: ContextAction,
    ) -> Self {
        Self {
            resolve_url,
            ..Self::from_uri(uri, fallback, update, action)
        }
    }

    pub fn from_context(context: Context, update: ContextType, action: ContextAction) -> Self {
        Self {
            resolve_url: lexicon_url(&context).map(str::to_string),
            resolve: Resolve::Context(context),
            fallback: None,
            update,
            action,
        }
    }

    /// the uri which should be used to resolve the context, might not be the context uri
    fn resolve_uri(&self) -> Option<&str> {
        // it's important to call this always, or at least for every ResolveContext
        // otherwise we might not even check if we need to fallback and just use the fallback uri
        match self.resolve {
            Resolve::Uri(ref uri) => ConnectState::valid_resolve_uri(uri),
            Resolve::Context(ref ctx) => {
                ConnectState::find_valid_uri(ctx.uri.as_deref(), ctx.pages.first())
            }
        }
        .or(self.fallback.as_deref())
    }

    /// the actual context uri
    fn context_uri(&self) -> &str {
        match self.resolve {
            Resolve::Uri(ref uri) => uri,
            Resolve::Context(ref ctx) => ctx.uri.as_deref().unwrap_or_default(),
        }
    }
}

impl Display for ResolveContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resolve_uri: <{:?}>, resolve_url: <{:?}>, context_uri: <{}>, update: <{:?}>",
            self.resolve_uri(),
            self.resolve_url,
            self.context_uri(),
            self.update,
        )
    }
}

#[derive(Debug, ThisError)]
enum ContextResolverError {
    #[error("no next context to resolve")]
    NoNext,
    #[error("tried appending context with {0} pages")]
    UnexpectedPagesSize(usize),
    #[error("tried resolving not allowed context: {0:?}")]
    NotAllowedContext(String),
}

impl From<ContextResolverError> for Error {
    fn from(value: ContextResolverError) -> Self {
        Error::failed_precondition(value)
    }
}

pub struct ContextResolver {
    session: Session,
    queue: VecDeque<ResolveContext>,
    unavailable_contexts: HashMap<ResolveContext, Instant>,
}

// time after which an unavailable context is retried
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(3600);

impl ContextResolver {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            queue: VecDeque::new(),
            unavailable_contexts: HashMap::new(),
        }
    }

    pub fn add(&mut self, resolve: ResolveContext) {
        let last_try = self
            .unavailable_contexts
            .get(&resolve)
            .map(Instant::elapsed);

        let last_try = if matches!(last_try, Some(last_try) if last_try > RETRY_UNAVAILABLE) {
            let _ = self.unavailable_contexts.remove(&resolve);
            debug!(
                "context was requested {}s ago, trying again to resolve the requested context",
                last_try.expect("checked by condition").as_secs()
            );
            None
        } else {
            last_try
        };

        if last_try.is_some() {
            debug!("tried loading unavailable context: {resolve}");
            return;
        } else if self.queue.contains(&resolve) {
            debug!("update for {resolve} is already added");
            return;
        } else {
            trace!(
                "added {} to resolver queue",
                resolve.resolve_uri().unwrap_or(resolve.context_uri())
            )
        }

        self.queue.push_back(resolve)
    }

    pub fn add_list(&mut self, resolve: Vec<ResolveContext>) {
        for resolve in resolve {
            self.add(resolve)
        }
    }

    pub fn remove_used_and_invalid(&mut self) {
        if let Some((_, _, remove)) = self.find_next() {
            let _ = self.queue.drain(0..remove); // remove invalid
        }
        self.queue.pop_front(); // remove used
    }

    pub fn clear(&mut self) {
        self.queue = VecDeque::new()
    }

    fn find_next(&self) -> Option<(&ResolveContext, &str, usize)> {
        for idx in 0..self.queue.len() {
            let next = self.queue.get(idx)?;
            match next.resolve_uri() {
                None => {
                    warn!("skipped {idx} because of invalid resolve_uri: {next}");
                    continue;
                }
                Some(uri) => return Some((next, uri, idx)),
            }
        }
        None
    }

    pub fn has_next(&self) -> bool {
        self.find_next().is_some()
    }

    pub async fn get_next_context(
        &self,
        recent_track_uri: impl Fn() -> Vec<String>,
    ) -> Result<Context, Error> {
        let (next, resolve_uri, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        match next.update {
            ContextType::Default => match next.resolve_url {
                Some(ref url) => {
                    let mut ctx = self.session.spclient().get_context_from_url(url).await;
                    if let Ok(ctx) = ctx.as_mut() {
                        ctx.uri = Some(next.context_uri().to_string());
                        // `url` and the requesting metadata are kept, not overwritten with
                        // `context://<uri>`: they are how a re-resolve finds the tracks again.
                        if let Resolve::Context(ref requested) = next.resolve {
                            for (key, value) in &requested.metadata {
                                ctx.metadata.insert(key.clone(), value.clone());
                            }
                        }
                    }

                    ctx
                }
                None => {
                    let mut ctx = self.session.spclient().get_context(resolve_uri).await;
                    if let Ok(ctx) = ctx.as_mut() {
                        ctx.uri = Some(next.context_uri().to_string());
                        ctx.url = ctx.uri.as_ref().map(|s| format!("context://{s}"));
                    }

                    ctx
                }
            },
            ContextType::Autoplay => {
                if resolve_uri.contains("spotify:show:") || resolve_uri.contains("spotify:episode:")
                {
                    // autoplay is not supported for podcasts
                    Err(ContextResolverError::NotAllowedContext(
                        resolve_uri.to_string(),
                    ))?
                }

                let request = AutoplayContextRequest {
                    context_uri: Some(resolve_uri.to_string()),
                    recent_track_uri: recent_track_uri(),
                    ..Default::default()
                };
                self.session.spclient().get_autoplay_context(&request).await
            }
        }
    }

    pub fn mark_next_unavailable(&mut self) {
        if let Some((next, _, _)) = self.find_next() {
            self.unavailable_contexts
                .insert(next.clone(), Instant::now());
        }
    }

    pub fn apply_next_context(
        &self,
        state: &mut ConnectState,
        mut context: Context,
    ) -> Result<Option<Vec<ResolveContext>>, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        let remaining = match next.action {
            ContextAction::Append if context.pages.len() == 1 => state
                .fill_context_from_page(context.pages.remove(0))
                .map(|_| None),
            ContextAction::Replace => {
                let remaining = state.update_context(context, next.update);
                if let Resolve::Context(ref ctx) = next.resolve {
                    state.merge_context(ctx.pages.clone().pop());
                }

                remaining
            }
            ContextAction::Append => {
                warn!("unexpected page size: {context:#?}");
                Err(ContextResolverError::UnexpectedPagesSize(context.pages.len()).into())
            }
        }?;

        let context_uri = state.context_uri().clone();

        Ok(remaining.map(|remaining| {
            remaining
                .into_iter()
                .map(|page| ResolveContext::append_context(&context_uri, page))
                .collect::<Vec<_>>()
        }))
    }

    pub fn try_finish(
        &self,
        state: &mut ConnectState,
        transfer_state: &mut Option<TransferState>,
    ) -> bool {
        let (next, _, _) = match self.find_next() {
            None => return false,
            Some(next) => next,
        };

        // when there is only one update type, we are the last of our kind, so we should update the state
        if self
            .queue
            .iter()
            .filter(|resolve| resolve.update == next.update)
            .count()
            != 1
        {
            return false;
        }

        match (next.update, state.active_context) {
            (ContextType::Default, ContextType::Default) | (ContextType::Autoplay, _) => {
                debug!(
                    "last item of type <{:?}>, finishing state setup",
                    next.update
                );
            }
            (ContextType::Default, _) => {
                debug!("skipped finishing default, because it isn't the active context");
                return false;
            }
        }

        let active_ctx = state.get_context(state.active_context);
        let res = if let Some(transfer_state) = transfer_state.take() {
            state.finish_transfer(transfer_state)
        } else if state.shuffling_context() && next.update == ContextType::Default {
            state.shuffle_new()
        } else if matches!(active_ctx, Ok(ctx) if ctx.index.track == 0) {
            // has context, and context is not touched
            // when the index is not zero, the next index was already evaluated elsewhere
            let ctx = active_ctx.expect("checked by precondition");
            let idx = ConnectState::find_index_in_context(ctx, |t| {
                state.current_track(|c| t.uri == c.uri)
            })
            .ok();

            state.reset_playback_to_position(idx)
        } else {
            state.fill_up_next_tracks()
        };

        if let Err(why) = res {
            error!("setup of state failed: {why}, last used resolve {next:#?}")
        }

        state.update_restrictions();
        state.update_queue_revision();

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HM_URL: &str = "hm://lexicon-session-provider/context-resolve/v2/session?contextUri=spotify:playlist:37i9dQZF1EYkqdzj48dyYq";
    const DJ_URI: &str = "spotify:playlist:37i9dQZF1EYkqdzj48dyYq";

    fn context(url: Option<&str>, lexicon: Option<&str>) -> Context {
        let mut context = Context {
            uri: Some(DJ_URI.into()),
            url: url.map(str::to_string),
            ..Default::default()
        };

        if let Some(lexicon) = lexicon {
            context
                .metadata
                .insert(LEXICON_CONTEXT_URL.into(), lexicon.into());
        }

        context
    }

    #[test]
    fn lexicon_url_prefers_the_context_url() {
        assert_eq!(
            lexicon_url(&context(Some(HM_URL), Some("hm://other"))),
            Some(HM_URL)
        );
    }

    #[test]
    fn lexicon_url_falls_back_to_the_metadata() {
        let ctx = context(Some("context://spotify:playlist:x"), Some(HM_URL));
        assert_eq!(lexicon_url(&ctx), Some(HM_URL));
    }

    #[test]
    fn lexicon_url_ignores_an_ordinary_context() {
        let ctx = context(Some("context://spotify:playlist:x"), None);
        assert_eq!(lexicon_url(&ctx), None);
    }

    #[test]
    fn from_context_carries_the_lexicon_url() {
        let resolve = ResolveContext::from_context(
            context(None, Some(HM_URL)),
            ContextType::Default,
            ContextAction::Replace,
        );

        assert_eq!(resolve.resolve_url.as_deref(), Some(HM_URL));
    }

    #[test]
    fn append_context_keeps_the_context_uri_for_a_page_url() {
        let resolve = ResolveContext::append_context(DJ_URI, PageRef::Url(HM_URL.into()));

        assert_eq!(resolve.context_uri(), DJ_URI);
        assert_eq!(resolve.resolve_url.as_deref(), Some(HM_URL));
    }

    #[test]
    fn append_context_resolves_a_page_uri_by_uri() {
        let page_uri = "spotify:album:5LFzwirfFwBKXJQGfwmiMY";
        let resolve = ResolveContext::append_context(
            "spotify:artist:5LFzwirfFwBKXJQGfwmiMY",
            PageRef::Uri(page_uri.into()),
        );

        assert_eq!(resolve.resolve_uri(), Some(page_uri));
        assert_eq!(resolve.resolve_url, None);
    }
}
