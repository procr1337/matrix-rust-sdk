// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A sub-object for running pagination tasks on a given room.

use std::{sync::Arc, time::Duration};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::{
    deserialized_responses::TimelineEvent, linked_chunk::ChunkIdentifier, timeout::timeout,
};
use matrix_sdk_common::linked_chunk::ChunkContent;
use ruma::api::Direction;
use tokio::sync::RwLockWriteGuard;
use tracing::{debug, instrument, trace};

use super::{
    deduplicator::DeduplicationOutcome,
    room::{
        events::{Gap, RoomEvents},
        LoadMoreEventsBackwardsOutcome, RoomEventCacheInner,
    },
    BackPaginationOutcome, EventsOrigin, Result, RoomEventCacheState, RoomEventCacheUpdate,
};
use crate::{event_cache::EventCacheError, room::MessagesOptions};

/// Status for the back-pagination on a room event cache.
#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RoomPaginationStatus {
    /// No back-pagination is happening right now.
    Idle {
        /// Have we hit the start of the timeline, i.e. back-paginating wouldn't
        /// have any effect?
        hit_timeline_start: bool,
    },

    /// Back-pagination is already running in the background.
    Paginating,
}

/// Small RAII guard to reset the pagination status on drop, if not disarmed in
/// the meanwhile.
struct ResetStatusOnDrop {
    prev_status: Option<RoomPaginationStatus>,
    pagination_status: SharedObservable<RoomPaginationStatus>,
}

impl ResetStatusOnDrop {
    /// Make the RAII guard have no effect.
    fn disarm(mut self) {
        self.prev_status = None;
    }
}

impl Drop for ResetStatusOnDrop {
    fn drop(&mut self) {
        if let Some(status) = self.prev_status.take() {
            let _ = self.pagination_status.set(status);
        }
    }
}

/// An API object to run pagination queries on a [`super::RoomEventCache`].
///
/// Can be created with [`super::RoomEventCache::pagination()`].
#[allow(missing_debug_implementations)]
#[derive(Clone)]
pub struct RoomPagination {
    pub(super) inner: Arc<RoomEventCacheInner>,
}

impl RoomPagination {
    /// Starts a back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    ///
    /// It will run multiple back-paginations until one of these two conditions
    /// is met:
    /// - either we've reached the start of the timeline,
    /// - or we've obtained enough events to fulfill the requested number of
    ///   events.
    #[instrument(skip(self))]
    pub async fn run_backwards_until(
        &self,
        num_requested_events: u16,
    ) -> Result<BackPaginationOutcome> {
        let mut events = Vec::new();

        loop {
            if let Some(outcome) = self.run_backwards_impl(num_requested_events).await? {
                events.extend(outcome.events);
                if outcome.reached_start || events.len() >= num_requested_events as usize {
                    return Ok(BackPaginationOutcome {
                        reached_start: outcome.reached_start,
                        events,
                    });
                }
                trace!("restarting back-pagination, because we haven't reached the start or obtained enough events yet");
            }

            debug!("restarting back-pagination because of a timeline reset.");
        }
    }

    /// Run a single back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    #[instrument(skip(self))]
    pub async fn run_backwards_once(&self, batch_size: u16) -> Result<BackPaginationOutcome> {
        loop {
            if let Some(outcome) = self.run_backwards_impl(batch_size).await? {
                return Ok(outcome);
            }
            debug!("restarting back-pagination because of a timeline reset.");
        }
    }

    /// Paginate from either the storage or the network, and let pagination
    /// status observers know about updates.
    async fn run_backwards_impl(&self, batch_size: u16) -> Result<Option<BackPaginationOutcome>> {
        // There is at least one gap that must be resolved; reach the network.
        // First, ensure there's no other ongoing back-pagination.
        let status_observable = &self.inner.pagination_status;

        let prev_status = status_observable.set(RoomPaginationStatus::Paginating);
        if !matches!(prev_status, RoomPaginationStatus::Idle { .. }) {
            return Err(EventCacheError::AlreadyBackpaginating);
        }

        let reset_status_on_drop_guard = ResetStatusOnDrop {
            prev_status: Some(prev_status),
            pagination_status: status_observable.clone(),
        };

        match self.paginate_backwards_impl(batch_size).await? {
            Some(outcome) => {
                // Back-pagination's over and successful, don't reset the status to the previous
                // value.
                reset_status_on_drop_guard.disarm();

                // Notify subscribers that pagination ended.
                status_observable
                    .set(RoomPaginationStatus::Idle { hit_timeline_start: outcome.reached_start });
                Ok(Some(outcome))
            }

            None => {
                // We keep the previous status value, because we haven't obtained more
                // information about the pagination.
                Ok(None)
            }
        }
    }

    /// Paginate from either the storage or the network.
    ///
    /// This method isn't concerned with setting the pagination status; only the
    /// caller is.
    async fn paginate_backwards_impl(
        &self,
        batch_size: u16,
    ) -> Result<Option<BackPaginationOutcome>> {
        // A linked chunk might not be entirely loaded (if it's been lazy-loaded). Try
        // to load from storage first, then from network if storage indicated
        // there's no previous events chunk to load.

        loop {
            let mut state_guard = self.inner.state.write().await;

            match state_guard.load_more_events_backwards().await? {
                LoadMoreEventsBackwardsOutcome::WaitForInitialPrevToken => {
                    const DEFAULT_WAIT_FOR_TOKEN_DURATION: Duration = Duration::from_secs(3);

                    // Release the state guard while waiting, to not deadlock the sync task.
                    drop(state_guard);

                    // Otherwise, wait for a notification that we received a previous-batch token.
                    trace!("waiting for a pagination token…");
                    let _ = timeout(
                        self.inner.pagination_batch_token_notifier.notified(),
                        DEFAULT_WAIT_FOR_TOKEN_DURATION,
                    )
                    .await;
                    trace!("done waiting");

                    self.inner.state.write().await.waited_for_initial_prev_token = true;

                    // Retry!
                    //
                    // Note: the next call to `load_more_events_backwards` can't return
                    // `WaitForInitialPrevToken` because we've just set to
                    // `waited_for_initial_prev_token`, so this is not an infinite loop.
                    //
                    // Note 2: not a recursive call, because recursive and async have a bad time
                    // together.
                    continue;
                }

                LoadMoreEventsBackwardsOutcome::Gap { prev_token } => {
                    // We have a gap, so resolve it with a network back-pagination.
                    drop(state_guard);
                    return self.paginate_backwards_with_network(batch_size, prev_token).await;
                }

                LoadMoreEventsBackwardsOutcome::StartOfTimeline => {
                    return Ok(Some(BackPaginationOutcome { reached_start: true, events: vec![] }));
                }

                LoadMoreEventsBackwardsOutcome::Events {
                    events,
                    timeline_event_diffs,
                    reached_start,
                } => {
                    if !timeline_event_diffs.is_empty() {
                        let _ =
                            self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                                diffs: timeline_event_diffs,
                                origin: EventsOrigin::Pagination,
                            });
                    }

                    return Ok(Some(BackPaginationOutcome {
                        reached_start,
                        // This is a backwards pagination. `BackPaginationOutcome` expects events to
                        // be in “reverse order”.
                        events: events.into_iter().rev().collect(),
                    }));
                }
            }
        }
    }

    /// Run a single pagination request (/messages) to the server.
    ///
    /// If there are no previous-batch tokens, it will wait for one for a short
    /// while to get one, or if it's already done so or if it's seen a
    /// previous-batch token before, it will immediately indicate it's
    /// reached the end of the timeline.
    async fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let (events, new_gap) = {
            let Some(room) = self.inner.weak_room.get() else {
                // The client is shutting down, return an empty default response.
                return Ok(Some(BackPaginationOutcome {
                    reached_start: false,
                    events: Default::default(),
                }));
            };

            let mut options = MessagesOptions::new(Direction::Backward).from(prev_token.as_deref());
            options.limit = batch_size.into();

            let response = room.messages(options).await.map_err(|err| {
                EventCacheError::BackpaginationError(
                    crate::event_cache::paginator::PaginatorError::SdkError(Box::new(err)),
                )
            })?;

            let new_gap = response.end.map(|prev_token| Gap { prev_token });

            (response.chunk, new_gap)
        };

        // Make sure the `RoomEvents` isn't updated while we are saving events from
        // backpagination.
        let state = self.inner.state.write().await;

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let prev_gap_chunk_id = if let Some(token) = prev_token {
            let gap_chunk_id = state.events().chunk_identifier(|chunk| {
                matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if *prev_token == token)
            });

            if gap_chunk_id.is_none() {
                // We got a previous-batch token from the linked chunk *before* running the
                // request, but it is missing *after* completing the
                // request.
                //
                // It may be a sign the linked chunk has been reset, but it's fine, per this
                // function's contract.
                return Ok(None);
            }

            gap_chunk_id
        } else {
            None
        };

        self.handle_network_pagination_result(state, events, new_gap, prev_gap_chunk_id)
            .await
            .map(Some)
    }

    /// Handle the result of a successful network back-pagination.
    async fn handle_network_pagination_result(
        &self,
        mut state: RwLockWriteGuard<'_, RoomEventCacheState>,
        events: Vec<TimelineEvent>,
        new_gap: Option<Gap>,
        prev_gap_id: Option<ChunkIdentifier>,
    ) -> Result<BackPaginationOutcome> {
        // If there's no new previous gap, then we've reached the start of the timeline.
        let reached_start = new_gap.is_none();

        let (
            DeduplicationOutcome {
                all_events: mut events,
                in_memory_duplicated_event_ids,
                in_store_duplicated_event_ids,
            },
            all_duplicates,
        ) = state.collect_valid_and_duplicated_events(events).await?;

        // During a backwards pagination, when a duplicated event is found, the old
        // event is kept and the new event is ignored. This is the opposite strategy
        // than during a sync where the old event is removed and the new event is added.
        if !all_duplicates {
            // Let's forget the new events that are duplicated.
            events.retain(|new_event| {
                new_event
                    .event_id()
                    .map(|event_id| {
                        !in_memory_duplicated_event_ids
                            .iter()
                            .chain(in_store_duplicated_event_ids.iter())
                            .any(|(duplicated_event_id, _position)| {
                                duplicated_event_id == &event_id
                            })
                    })
                    // Forget event with no ID, should be unreachable because of
                    // `collect_valid_and_duplicated_events` though.
                    .unwrap_or(false)
            });
        } else {
            // All new events are duplicated, they can all be ignored.
            events.clear();
        }

        let timeline_event_diffs = state
            .with_events_mut(|room_events| {
            // Reverse the order of the events as `/messages` has been called with `dir=b`
            // (backwards). The `RoomEvents` API expects the first event to be the oldest.
            // Let's re-order them for this block.
            let reversed_events = events
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>();

            let first_event_pos = room_events.events().next().map(|(item_pos, _)| item_pos);

            // First, insert events.
            let insert_new_gap_pos = if let Some(gap_id) = prev_gap_id {
                // There is a prior gap, let's replace it by new events!
                if all_duplicates {
                    // All the events were duplicated; don't act upon them, and only remove the
                    // prior gap that we just filled.
                    trace!("removing previous gap, as all events have been deduplicated");
                    room_events.remove_gap_at(gap_id).expect("gap identifier is a valid gap chunk id we read previously")
                } else {
                    trace!("replacing previous gap with the back-paginated events");

                    // Replace the gap with the events we just deduplicated.
                    room_events.replace_gap_at(reversed_events.clone(), gap_id)
                        .expect("gap_identifier is a valid chunk id we read previously")
                }
            } else if let Some(pos) = first_event_pos {
                // No prior gap, but we had some events: assume we need to prepend events
                // before those.
                trace!("inserted events before the first known event");

                room_events
                    .insert_events_at(reversed_events.clone(), pos)
                    .expect("pos is a valid position we just read above");

                Some(pos)
            } else {
                // No prior gap, and no prior events: push the events.
                trace!("pushing events received from back-pagination");

                room_events.push_events(reversed_events.clone());

                // A new gap may be inserted before the new events, if there are any.
                room_events.events().next().map(|(item_pos, _)| item_pos)
            };

            // And insert the new gap if needs be.
            //
            // We only do this when at least one new, non-duplicated event, has been added to
            // the chunk. Otherwise it means we've back-paginated all the known events.
            if !all_duplicates {
                if let Some(new_gap) = new_gap {
                    if let Some(new_pos) = insert_new_gap_pos {
                        room_events
                            .insert_gap_at(new_gap, new_pos)
                            .expect("events_chunk_pos represents a valid chunk position");
                    } else {
                        room_events.push_gap(new_gap);
                    }
                }
            } else {
                debug!("not storing previous batch token, because we deduplicated all new back-paginated events");
            }

            room_events.on_new_events(&self.inner.room_version, reversed_events.iter());
        })
        .await?;

        // There could be an inconsistency between the network (which thinks we hit the
        // start of the timeline) and the disk (which has the initial empty
        // chunks), so tweak the `reached_start` value so that it reflects the disk
        // state in priority instead.
        let reached_start = {
            // There are no gaps.
            !state.events().chunks().any(|chunk| chunk.is_gap()) &&
            // The first chunk has no predecessors.
            state.events()
            .chunks()
            .next()
            .map_or(reached_start, |chunk| chunk.is_definitive_head())
        };

        let backpagination_outcome = BackPaginationOutcome { events, reached_start };

        if !timeline_event_diffs.is_empty() {
            let _ = self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Pagination,
            });
        }

        Ok(backpagination_outcome)
    }

    /// Get the latest pagination token, as stored in the room events linked
    /// list, or wait for it for the given amount of time.
    ///
    /// It will only wait if we *never* saw an initial previous-batch token.
    /// Otherwise, it will immediately skip.
    #[doc(hidden)]
    pub async fn get_or_wait_for_token(&self, wait_time: Option<Duration>) -> PaginationToken {
        fn get_latest(events: &RoomEvents) -> Option<String> {
            events.rchunks().find_map(|chunk| match chunk.content() {
                ChunkContent::Gap(gap) => Some(gap.prev_token.clone()),
                ChunkContent::Items(..) => None,
            })
        }

        {
            // Scope for the lock guard.
            let state = self.inner.state.read().await;

            // Check if the linked chunk contains any events. If so, absence of a gap means
            // we've hit the start of the timeline. If not, absence of a gap
            // means we've never received a pagination token from sync, and we
            // should wait for one.
            let has_events = state.events().events().next().is_some();

            // Fast-path: we do have a previous-batch token already.
            if let Some(found) = get_latest(state.events()) {
                return PaginationToken::HasMore(found);
            }

            // If we had events, and there was no gap, then we've hit the end of the
            // timeline.
            if has_events {
                return PaginationToken::HitEnd;
            }

            // If we've already waited for an initial previous-batch token before,
            // immediately abort.
            if state.waited_for_initial_prev_token {
                return PaginationToken::None;
            }
        }

        // If the caller didn't set a wait time, return none early.
        let Some(wait_time) = wait_time else {
            return PaginationToken::None;
        };

        // Otherwise, wait for a notification that we received a previous-batch token.
        // Note the state lock is released while doing so, allowing other tasks to write
        // into the linked chunk.
        let _ = timeout(self.inner.pagination_batch_token_notifier.notified(), wait_time).await;

        let mut state = self.inner.state.write().await;

        state.waited_for_initial_prev_token = true;

        if let Some(token) = get_latest(state.events()) {
            PaginationToken::HasMore(token)
        } else if state.events().events().next().is_some() {
            // See logic above, in the read lock guard scope.
            PaginationToken::HitEnd
        } else {
            PaginationToken::None
        }
    }

    /// Returns a subscriber to the pagination status used for the
    /// back-pagination integrated to the event cache.
    pub fn status(&self) -> Subscriber<RoomPaginationStatus> {
        self.inner.pagination_status.subscribe()
    }
}

/// Pagination token data, indicating in which state is the current pagination.
#[derive(Clone, Debug, PartialEq)]
pub enum PaginationToken {
    /// We never had a pagination token, so we'll start back-paginating from the
    /// end, or forward-paginating from the start.
    None,
    /// We paginated once before, and we received a prev/next batch token that
    /// we may reuse for the next query.
    HasMore(String),
    /// We've hit one end of the timeline (either the start or the actual end),
    /// so there's no need to continue paginating.
    HitEnd,
}

impl From<Option<String>> for PaginationToken {
    fn from(token: Option<String>) -> Self {
        match token {
            Some(val) => Self::HasMore(val),
            None => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    // Those tests require time to work, and it does not on wasm32.
    #[cfg(not(target_arch = "wasm32"))]
    mod time_tests {
        use std::time::{Duration, Instant};

        use assert_matches::assert_matches;
        use matrix_sdk_base::RoomState;
        use matrix_sdk_test::{async_test, event_factory::EventFactory, ALICE};
        use ruma::{event_id, room_id, user_id};
        use tokio::{spawn, time::sleep};

        use crate::{
            event_cache::{pagination::PaginationToken, room::events::Gap},
            test_utils::logged_in_client,
        };

        #[async_test]
        async fn test_wait_no_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();

            let pagination = room_event_cache.pagination();

            // If I have a room with no events, and try to get a pagination token without
            // waiting,
            let found = pagination.get_or_wait_for_token(None).await;
            // Then I don't get any pagination token.
            assert_matches!(found, PaginationToken::None);

            // Reset waited_for_initial_prev_token and event state.
            let _ = pagination.inner.state.write().await.reset().await.unwrap();

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I don't get any,
            assert_matches!(found, PaginationToken::None);
            // and I haven't waited long.
            assert!(waited.as_secs() < 1);

            // Reset waited_for_initial_prev_token state.
            let _ = pagination.inner.state.write().await.reset().await.unwrap();

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(1))).await;
            let waited = before.elapsed();
            // then I still don't get any.
            assert_matches!(found, PaginationToken::None);
            // and I've waited a bit.
            assert!(waited.as_secs() < 2);
            assert!(waited.as_secs() >= 1);
        }

        #[async_test]
        async fn test_wait_hit_end_of_timeline() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();

            let f = EventFactory::new().room(room_id).sender(*ALICE);
            let pagination = room_event_cache.pagination();

            // Add a previous event.
            room_event_cache
                .inner
                .state
                .write()
                .await
                .with_events_mut(|events| {
                    events.push_events([f
                        .text_msg("this is the start of the timeline")
                        .into_event()]);
                })
                .await
                .unwrap();

            // If I have a room with events, and try to get a pagination token without
            // waiting,
            let found = pagination.get_or_wait_for_token(None).await;
            // I've reached the start of the timeline.
            assert_matches!(found, PaginationToken::HitEnd);

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // Then I still have reached the start of the timeline.
            assert_matches!(found, PaginationToken::HitEnd);
            // and I've waited very little.
            assert!(waited.as_secs() < 1);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(1))).await;
            let waited = before.elapsed();
            // then I still don't get any.
            assert_matches!(found, PaginationToken::HitEnd);
            // and I've waited very little (there's no point in waiting in this case).
            assert!(waited.as_secs() < 1);

            // Now, reset state. We'll add an event *after* we've started waiting, this
            // time.
            room_event_cache.clear().await.unwrap();

            spawn(async move {
                sleep(Duration::from_secs(1)).await;

                room_event_cache
                    .inner
                    .state
                    .write()
                    .await
                    .with_events_mut(|events| {
                        events.push_events([f
                            .text_msg("this is the start of the timeline")
                            .into_event()]);
                    })
                    .await
                    .unwrap();
            });

            // If I wait for a pagination token,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(2))).await;
            let waited = before.elapsed();
            // since sync has returned all events, and no prior gap, I've hit the end.
            assert_matches!(found, PaginationToken::HitEnd);
            // and I've waited for the whole duration.
            assert!(waited.as_secs() >= 2);
            assert!(waited.as_secs() < 3);
        }

        #[async_test]
        async fn test_wait_for_pagination_token_already_present() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();

            let expected_token = "old".to_owned();

            // When I have events and multiple gaps, in a room,
            {
                room_event_cache
                    .inner
                    .state
                    .write()
                    .await
                    .with_events_mut(|room_events| {
                        room_events.push_gap(Gap { prev_token: expected_token.clone() });
                        room_events.push_events([EventFactory::new()
                            .text_msg("yolo")
                            .sender(user_id!("@b:z.h"))
                            .event_id(event_id!("$ida"))
                            .into_event()]);
                    })
                    .await
                    .unwrap();
            }

            let pagination = room_event_cache.pagination();

            // If I don't wait for a back-pagination token,
            let found = pagination.get_or_wait_for_token(None).await;
            // Then I get it.
            assert_eq!(found, PaginationToken::HasMore(expected_token.clone()));

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found, PaginationToken::HasMore(expected_token.clone()));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(1))).await;
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found, PaginationToken::HasMore(expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);
        }

        #[async_test]
        async fn test_wait_for_late_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();

            let expected_token = "old".to_owned();

            let before = Instant::now();
            let cloned_expected_token = expected_token.clone();
            let cloned_room_event_cache = room_event_cache.clone();
            let insert_token_task = spawn(async move {
                // If a backpagination token is inserted after 400 milliseconds,
                sleep(Duration::from_millis(400)).await;

                cloned_room_event_cache
                    .inner
                    .state
                    .write()
                    .await
                    .with_events_mut(|events| {
                        events.push_gap(Gap { prev_token: cloned_expected_token })
                    })
                    .await
                    .unwrap();
            });

            let pagination = room_event_cache.pagination();

            // Then first I don't get it (if I'm not waiting,)
            let found = pagination.get_or_wait_for_token(None).await;
            assert_matches!(found, PaginationToken::None);

            // And if I wait for the back-pagination token for 600ms,
            let found = pagination.get_or_wait_for_token(Some(Duration::from_millis(600))).await;
            let waited = before.elapsed();

            // then I do get one eventually.
            assert_eq!(found, PaginationToken::HasMore(expected_token));
            // and I have waited between ~400 and ~1000 milliseconds.
            assert!(waited.as_secs() < 1);
            assert!(waited.as_millis() >= 400);

            // The task succeeded.
            insert_token_task.await.unwrap();
        }

        #[async_test]
        async fn test_get_latest_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();

            let old_token = "old".to_owned();
            let new_token = "new".to_owned();

            // Assuming a room event cache that contains both an old and a new pagination
            // token, and events in between,
            room_event_cache
                .inner
                .state
                .write()
                .await
                .with_events_mut(|events| {
                    let f = EventFactory::new().room(room_id).sender(*ALICE);

                    // This simulates a valid representation of a room: first group of gap+events
                    // were e.g. restored from the cache; second group of gap+events was received
                    // from a subsequent sync.
                    events.push_gap(Gap { prev_token: old_token });
                    events.push_events([f.text_msg("oldest from cache").into()]);

                    events.push_gap(Gap { prev_token: new_token.clone() });
                    events.push_events([f.text_msg("sync'd gappy timeline").into()]);
                })
                .await
                .unwrap();

            let pagination = room_event_cache.pagination();

            // Retrieving the pagination token will return the most recent one, not the old
            // one.
            let found = pagination.get_or_wait_for_token(None).await;
            assert_eq!(found, PaginationToken::HasMore(new_token));
        }
    }
}
