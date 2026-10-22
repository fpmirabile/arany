use super::*;

pub(crate) struct CheckReducer;
impl CheckReducer {
    pub(crate) fn apply(
        id: CheckId,
        view: Option<CheckView>,
        event: &CheckEvent,
    ) -> Result<CheckView, StoreError> {
        if !id.valid() {
            return Err(StoreError::InvalidHistory);
        }
        let Some(mut view) = view else {
            let CheckEvent::ProviderCheckStarted { start } = event else {
                return Err(StoreError::InvalidHistory);
            };
            if !start.valid() {
                return Err(StoreError::InvalidHistory);
            }
            return Ok(CheckView {
                id,
                start: start.clone(),
                events: 1,
                completed_calls: 0,
                pending: None,
                disposition: None,
                cleared: false,
                catalog_pages: 0,
                catalog_complete: start.catalog_pages == 0,
                probes: 0,
                failed: None,
                publication: None,
                last_time_remaining_ms: LIFETIME_MS - SETTLEMENT_MS,
            });
        };
        if view.id != id || view.disposition.is_some() || view.events >= MAX_EVENTS {
            return Err(StoreError::InvalidHistory);
        }
        let valid = match event {
            CheckEvent::ProviderCheckStarted { .. } => false,
            CheckEvent::ProviderCheckEvidenceCleared => {
                let valid = !view.cleared && view.completed_calls == 0 && view.pending.is_none();
                view.cleared = true;
                valid
            }
            CheckEvent::ProviderCheckCallStarted {
                ordinal,
                kind,
                destination,
                calls_remaining,
                output_remaining,
                time_remaining_ms,
            } => {
                let expected = if !view.catalog_complete {
                    Some(CheckCallKind::Catalog)
                } else {
                    view.start.probes.get(view.probes).map(|p| p.kind)
                };
                let valid = view.cleared
                    && view.pending.is_none()
                    && view.failed.is_none()
                    && view.publication.is_none()
                    && *ordinal == view.completed_calls
                    && Some(*kind) == expected
                    && (*kind != CheckCallKind::Catalog
                        || view.catalog_pages < view.start.catalog_pages)
                    && *destination == view.start.destination
                    && *calls_remaining == view.calls_remaining()
                    && *output_remaining == view.output_remaining()
                    && *time_remaining_ms > 0
                    && *time_remaining_ms <= view.last_time_remaining_ms;
                view.pending = Some((*ordinal, *kind));
                view.last_time_remaining_ms = *time_remaining_ms;
                valid
            }
            CheckEvent::ProviderCheckCallFinished {
                ordinal,
                disposition,
                usage,
                catalog_complete,
            } => {
                let Some((pending, kind)) = view.pending else {
                    return Err(StoreError::InvalidHistory);
                };
                let successful = *disposition == CheckDisposition::Succeeded;
                let valid_usage = usage.is_none_or(|usage| {
                    usage.input_tokens <= crate::provider::MAX_REPORTED_INPUT_TOKENS
                        && usage.output_tokens
                            <= view
                                .start
                                .probes
                                .get(view.probes)
                                .map_or(0, |p| p.output_tokens)
                });
                let valid = pending == *ordinal
                    && valid_usage
                    && (kind != CheckCallKind::Catalog || usage.is_none())
                    && (kind == CheckCallKind::Catalog || !*catalog_complete)
                    && (!*catalog_complete || successful)
                    && (!successful
                        || kind == CheckCallKind::Catalog
                        || usage.is_some_and(|u| u.input_tokens > 0 && u.output_tokens > 0));
                if kind == CheckCallKind::Catalog {
                    view.catalog_pages += 1;
                    view.catalog_complete = *catalog_complete;
                } else if successful {
                    view.probes += 1;
                }
                if !successful {
                    view.failed = Some(*disposition);
                }
                view.completed_calls += 1;
                view.pending = None;
                valid
            }
            CheckEvent::ProviderCheckEvidenceStarted { snapshot, digest } => {
                let valid = view.cleared
                    && view.catalog_complete
                    && view.probes == view.start.probes.len()
                    && view.pending.is_none()
                    && view.failed.is_none()
                    && view.publication.is_none()
                    && *snapshot == view.start.fingerprint
                    && *digest != [0; 32];
                view.publication = Some(*digest);
                valid
            }
            CheckEvent::ProviderCheckFinished {
                disposition,
                evidence,
            } => {
                let valid = view.pending.is_none()
                    && match disposition {
                        CheckDisposition::Succeeded => {
                            view.failed.is_none()
                                && view.publication.is_some()
                                && *evidence == view.publication
                        }
                        CheckDisposition::Uncertain => evidence.is_none(),
                        CheckDisposition::Failed
                        | CheckDisposition::Rejected
                        | CheckDisposition::Cancelled => {
                            evidence.is_none()
                                && view.publication.is_none()
                                && view.failed != Some(CheckDisposition::Uncertain)
                        }
                    };
                view.disposition = Some(*disposition);
                valid
            }
        };
        if !valid {
            return Err(StoreError::InvalidHistory);
        }
        view.events += 1;
        Ok(view)
    }
}
