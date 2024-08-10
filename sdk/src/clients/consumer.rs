use crate::client::Client;
use crate::consumer::{Consumer, ConsumerKind};
use crate::diagnostic::DiagnosticEvent;
use crate::error::IggyError;
use crate::identifier::{IdKind, Identifier};
use crate::locking::{IggySharedMut, IggySharedMutFn};
use crate::messages::poll_messages::{PollingKind, PollingStrategy};
use crate::models::messages::{PolledMessage, PolledMessages};
use crate::utils::crypto::Encryptor;
use crate::utils::duration::IggyDuration;
use crate::utils::timestamp::IggyTimestamp;
use bytes::Bytes;
use dashmap::DashMap;
use futures::Stream;
use futures_util::{FutureExt, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, trace, warn};

const EMPTY_MESSAGES: Vec<PolledMessage> = Vec::new();

const ORDERING: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;
type PollMessagesFuture = Pin<Box<dyn Future<Output = Result<PolledMessages, IggyError>>>>;

/// The auto-commit configuration for storing the offset on the server.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum AutoCommit {
    /// The auto-commit is disabled and the offset must be stored manually by the consumer.
    Disabled,
    /// The auto-commit is enabled and the offset is stored on the server after a certain interval.
    Interval(IggyDuration),
    /// The auto-commit is enabled and the offset is stored on the server after a certain interval or depending on the mode.
    IntervalOrWhen(IggyDuration, AutoCommitWhen),
    /// The auto-commit is enabled and the offset is stored on the server depending on the mode.
    When(AutoCommitWhen),
}

/// The auto-commit mode for storing the offset on the server.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum AutoCommitWhen {
    /// The offset is stored on the server when the messages are received.
    PollingMessages,
    /// The offset is stored on the server when all the messages are consumed.
    ConsumingAllMessages,
    /// The offset is stored on the server when consuming each message.
    ConsumingEachMessage,
    /// The offset is stored on the server when consuming every Nth message.
    ConsumingEveryNthMessage(u32),
}

unsafe impl Send for IggyConsumer {}
unsafe impl Sync for IggyConsumer {}

pub struct IggyConsumer {
    initialized: bool,
    can_poll: Arc<AtomicBool>,
    client: IggySharedMut<Box<dyn Client>>,
    consumer_name: String,
    consumer: Arc<Consumer>,
    is_consumer_group: bool,
    joined_consumer_group: Arc<AtomicBool>,
    stream_id: Arc<Identifier>,
    topic_id: Arc<Identifier>,
    partition_id: Option<u32>,
    polling_strategy: PollingStrategy,
    poll_interval_micros: u64,
    batch_size: u32,
    auto_commit: AutoCommit,
    auto_commit_after_polling: bool,
    auto_join_consumer_group: bool,
    create_consumer_group_if_not_exists: bool,
    last_stored_offsets: Arc<DashMap<u32, AtomicU64>>,
    last_consumed_offsets: Arc<DashMap<u32, AtomicU64>>,
    current_offsets: Arc<DashMap<u32, AtomicU64>>,
    poll_future: Option<PollMessagesFuture>,
    buffered_messages: VecDeque<PolledMessage>,
    encryptor: Option<Arc<dyn Encryptor>>,
    store_offset_sender: flume::Sender<(u32, u64)>,
    store_offset_after_each_message: bool,
    store_offset_after_all_messages: bool,
    store_after_every_nth_message: u64,
    last_polled_at: Arc<AtomicU64>,
    current_partition_id: Arc<AtomicU32>,
    retry_interval: IggyDuration,
}

impl IggyConsumer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: IggySharedMut<Box<dyn Client>>,
        consumer_name: String,
        consumer: Consumer,
        stream_id: Identifier,
        topic_id: Identifier,
        partition_id: Option<u32>,
        polling_interval: Option<IggyDuration>,
        polling_strategy: PollingStrategy,
        batch_size: u32,
        auto_commit: AutoCommit,
        auto_join_consumer_group: bool,
        create_consumer_group_if_not_exists: bool,
        encryptor: Option<Arc<dyn Encryptor>>,
        retry_interval: IggyDuration,
    ) -> Self {
        let (store_offset_sender, _) = flume::unbounded();
        Self {
            initialized: false,
            is_consumer_group: consumer.kind == ConsumerKind::ConsumerGroup,
            joined_consumer_group: Arc::new(AtomicBool::new(false)),
            can_poll: Arc::new(AtomicBool::new(true)),
            client,
            consumer_name,
            consumer: Arc::new(consumer),
            stream_id: Arc::new(stream_id),
            topic_id: Arc::new(topic_id),
            partition_id,
            polling_strategy,
            poll_interval_micros: polling_interval.map_or(0, |interval| interval.as_micros()),
            last_stored_offsets: Arc::new(DashMap::new()),
            last_consumed_offsets: Arc::new(DashMap::new()),
            current_offsets: Arc::new(DashMap::new()),
            poll_future: None,
            batch_size,
            auto_commit,
            auto_commit_after_polling: matches!(
                auto_commit,
                AutoCommit::When(AutoCommitWhen::PollingMessages)
                    | AutoCommit::IntervalOrWhen(_, AutoCommitWhen::PollingMessages)
            ),
            auto_join_consumer_group,
            create_consumer_group_if_not_exists,
            buffered_messages: VecDeque::new(),
            encryptor,
            store_offset_sender,
            store_offset_after_each_message: matches!(
                auto_commit,
                AutoCommit::When(AutoCommitWhen::ConsumingEachMessage)
                    | AutoCommit::IntervalOrWhen(_, AutoCommitWhen::ConsumingEachMessage)
            ),
            store_offset_after_all_messages: matches!(
                auto_commit,
                AutoCommit::When(AutoCommitWhen::ConsumingAllMessages)
                    | AutoCommit::IntervalOrWhen(_, AutoCommitWhen::ConsumingAllMessages)
            ),
            store_after_every_nth_message: match auto_commit {
                AutoCommit::When(AutoCommitWhen::ConsumingEveryNthMessage(n))
                | AutoCommit::IntervalOrWhen(_, AutoCommitWhen::ConsumingEveryNthMessage(n)) => {
                    n as u64
                }
                _ => 0,
            },
            last_polled_at: Arc::new(AtomicU64::new(0)),
            current_partition_id: Arc::new(AtomicU32::new(0)),
            retry_interval,
        }
    }

    pub fn stream(&self) -> &Identifier {
        &self.stream_id
    }

    pub fn topic(&self) -> &Identifier {
        &self.topic_id
    }

    /// Initializes the consumer by subscribing to diagnostic events, initializing the consumer group if needed, storing the offsets in the background etc.
    pub async fn init(&mut self) -> Result<(), IggyError> {
        if self.initialized {
            return Ok(());
        }

        self.subscribe_events().await;
        self.init_consumer_group().await?;

        match self.auto_commit {
            AutoCommit::Interval(interval) => self.store_offsets_in_background(interval),
            AutoCommit::IntervalOrWhen(interval, _) => self.store_offsets_in_background(interval),
            _ => {}
        }

        let client = self.client.clone();
        let consumer = self.consumer.clone();
        let stream_id = self.stream_id.clone();
        let topic_id = self.topic_id.clone();
        let last_stored_offset = self.last_stored_offsets.clone();
        let (store_offset_sender, store_offset_receiver) = flume::unbounded();
        self.store_offset_sender = store_offset_sender;

        tokio::spawn(async move {
            while let Ok((partition_id, offset)) = store_offset_receiver.recv_async().await {
                trace!("Received offset to store: {offset}, partition ID: {partition_id}");
                let stored_offset;
                if let Some(offset_entry) = last_stored_offset.get(&partition_id) {
                    stored_offset = offset_entry.value().load(ORDERING);
                } else {
                    stored_offset = 0;
                    last_stored_offset.insert(partition_id, AtomicU64::new(0));
                }

                if offset <= stored_offset && offset >= 1 {
                    trace!("Offset: {offset} is less than or equal to the last stored offset: {stored_offset}. Skipping storing the offset.");
                    continue;
                }

                let client = client.read().await;
                if let Err(error) = client
                    .store_consumer_offset(
                        &consumer,
                        &stream_id,
                        &topic_id,
                        Some(partition_id),
                        offset,
                    )
                    .await
                {
                    error!("Failed to store offset: {offset}, error: {error}");
                    continue;
                }
                trace!("Stored offset: {offset}");
                if let Some(last_offset_entry) = last_stored_offset.get(&partition_id) {
                    last_offset_entry.value().store(offset, ORDERING);
                } else {
                    last_stored_offset.insert(partition_id, AtomicU64::new(offset));
                }
            }
        });

        self.initialized = true;
        Ok(())
    }

    fn store_offset(&self, partition_id: u32, offset: u64) {
        if let Err(error) = self.store_offset_sender.send((partition_id, offset)) {
            error!("Failed to send offset to store: {error}");
        }
    }

    fn store_offsets_in_background(&self, interval: IggyDuration) {
        let client = self.client.clone();
        let consumer = self.consumer.clone();
        let stream_id = self.stream_id.clone();
        let topic_id = self.topic_id.clone();
        let last_consumed_offset = self.last_consumed_offsets.clone();
        let last_stored_offset = self.last_stored_offsets.clone();
        tokio::spawn(async move {
            loop {
                sleep(interval.get_duration()).await;
                for entry in last_consumed_offset.iter() {
                    let partition_id = *entry.key();
                    let consumed_offset = entry.value().load(ORDERING);
                    let stored_offset = last_stored_offset
                        .get(&partition_id)
                        .map_or(0, |offset| offset.load(ORDERING));
                    trace!(
                        "Trying to store the offset: {consumed_offset}, last stored offset: {stored_offset} for partition ID: {partition_id}"
                    );

                    if consumed_offset <= stored_offset && consumed_offset >= 1 {
                        trace!("Offset: {consumed_offset} is less than or equal to the last stored offset: {stored_offset} for partition ID: {partition_id}. Skipping storing the offset in the background.");
                        continue;
                    }

                    let client = client.read().await;
                    if let Err(error) = client
                        .store_consumer_offset(
                            &consumer,
                            &stream_id,
                            &topic_id,
                            Some(partition_id),
                            consumed_offset,
                        )
                        .await
                    {
                        error!(
                            "Failed to store offset: {consumed_offset} for partition ID: {partition_id} in the background, error: {error}"
                        );
                        continue;
                    }
                    trace!("Stored offset: {consumed_offset} for partition ID: {partition_id} in the background.");
                    if let Some(stored_offset) = last_stored_offset.get(&partition_id) {
                        stored_offset.store(consumed_offset, ORDERING);
                    } else {
                        last_stored_offset.insert(partition_id, AtomicU64::new(consumed_offset));
                    }
                }
            }
        });
    }

    async fn init_consumer_group(&self) -> Result<(), IggyError> {
        if !self.is_consumer_group {
            return Ok(());
        }

        if !self.auto_join_consumer_group {
            warn!("Auto join consumer group is disabled");
            return Ok(());
        }

        Self::initialize_consumer_group(
            self.client.clone(),
            self.create_consumer_group_if_not_exists,
            self.stream_id.clone(),
            self.topic_id.clone(),
            self.consumer.clone(),
            &self.consumer_name,
            self.joined_consumer_group.clone(),
        )
        .await
    }

    async fn subscribe_events(&self) {
        trace!("Subscribing to diagnostic events");
        let mut receiver;
        {
            let client = self.client.read().await;
            receiver = client.subscribe_events().await;
        }

        let is_consumer_group = self.is_consumer_group;
        let can_join_consumer_group = is_consumer_group && self.auto_join_consumer_group;
        let client = self.client.clone();
        let create_consumer_group_if_not_exists = self.create_consumer_group_if_not_exists;
        let stream_id = self.stream_id.clone();
        let topic_id = self.topic_id.clone();
        let consumer = self.consumer.clone();
        let consumer_name = self.consumer_name.clone();
        let can_poll = self.can_poll.clone();
        let joined_consumer_group = self.joined_consumer_group.clone();
        let mut reconnected = false;
        let mut disconnected = false;

        tokio::spawn(async move {
            while let Some(event) = receiver.next().await {
                trace!("Received diagnostic event: {event}");
                match event {
                    DiagnosticEvent::Connected => {
                        trace!("Connected to the server");
                        joined_consumer_group.store(false, ORDERING);
                        if disconnected {
                            reconnected = true;
                            disconnected = false;
                        }
                    }
                    DiagnosticEvent::Disconnected => {
                        disconnected = true;
                        reconnected = false;
                        joined_consumer_group.store(false, ORDERING);
                        can_poll.store(false, ORDERING);
                        warn!("Disconnected from the server");
                    }
                    DiagnosticEvent::SignedIn => {
                        if !is_consumer_group {
                            can_poll.store(true, ORDERING);
                            continue;
                        }

                        if !can_join_consumer_group {
                            can_poll.store(true, ORDERING);
                            trace!("Auto join consumer group is disabled");
                            continue;
                        }

                        if !reconnected {
                            can_poll.store(true, ORDERING);
                            continue;
                        }

                        if joined_consumer_group.load(ORDERING) {
                            can_poll.store(true, ORDERING);
                            continue;
                        }

                        info!("Rejoining consumer group");
                        if let Err(error) = Self::initialize_consumer_group(
                            client.clone(),
                            create_consumer_group_if_not_exists,
                            stream_id.clone(),
                            topic_id.clone(),
                            consumer.clone(),
                            &consumer_name,
                            joined_consumer_group.clone(),
                        )
                        .await
                        {
                            error!("Failed to join consumer group: {error}");
                            continue;
                        }
                        info!("Rejoined consumer group");
                        can_poll.store(true, ORDERING);
                    }
                    DiagnosticEvent::SignedOut => {
                        joined_consumer_group.store(false, ORDERING);
                        can_poll.store(false, ORDERING);
                    }
                }
            }
        });
    }

    fn create_poll_messages_future(
        &self,
    ) -> impl Future<Output = Result<PolledMessages, IggyError>> {
        let stream_id = self.stream_id.clone();
        let topic_id = self.topic_id.clone();
        let partition_id = self.partition_id;
        let consumer = self.consumer.clone();
        let polling_strategy = self.polling_strategy;
        let client = self.client.clone();
        let count = self.batch_size;
        let auto_commit_after_polling = self.auto_commit_after_polling;
        let auto_commit_enabled = self.auto_commit != AutoCommit::Disabled;
        let interval = self.poll_interval_micros;
        let last_polled_at = self.last_polled_at.clone();
        let can_poll = self.can_poll.clone();
        let retry_interval = self.retry_interval;
        let last_stored_offset = self.last_stored_offsets.clone();
        let last_consumed_offset = self.last_consumed_offsets.clone();

        async move {
            if interval > 0 {
                Self::wait_before_polling(interval, last_polled_at.load(ORDERING)).await;
            }

            if !can_poll.load(ORDERING) {
                trace!("Trying to poll messages in {retry_interval}...");
                sleep(retry_interval.get_duration()).await;
            }

            trace!("Sending poll messages request");
            last_polled_at.store(IggyTimestamp::now().into(), ORDERING);
            let polled_messages = client
                .read()
                .await
                .poll_messages(
                    &stream_id,
                    &topic_id,
                    partition_id,
                    &consumer,
                    &polling_strategy,
                    count,
                    auto_commit_after_polling,
                )
                .await;

            if let Ok(polled_messages) = polled_messages {
                if polled_messages.messages.is_empty() {
                    return Ok(polled_messages);
                }

                let partition_id = polled_messages.partition_id;
                let consumed_offset;
                let has_consumed_offset;
                if let Some(offset_entry) = last_consumed_offset.get(&partition_id) {
                    has_consumed_offset = true;
                    consumed_offset = offset_entry.value().load(ORDERING);
                } else {
                    consumed_offset = 0;
                    has_consumed_offset = false;
                    last_consumed_offset.insert(partition_id, AtomicU64::new(0));
                }

                let stored_offset;
                if let Some(stored_offset_entry) = last_stored_offset.get(&partition_id) {
                    if auto_commit_after_polling {
                        stored_offset_entry.store(consumed_offset, ORDERING);
                        stored_offset = consumed_offset;
                    } else {
                        stored_offset = stored_offset_entry.value().load(ORDERING);
                    }
                } else {
                    if auto_commit_after_polling {
                        stored_offset = consumed_offset;
                    } else {
                        stored_offset = 0;
                    }
                    last_stored_offset.insert(partition_id, AtomicU64::new(stored_offset));
                }

                trace!(
                    "Last consumed offset: {consumed_offset}, current offset: {}, stored offset: {stored_offset}, in partition ID: {partition_id}, topic: {topic_id}, stream: {stream_id}, consumer: {consumer}",
                    polled_messages.current_offset
                );

                if has_consumed_offset && polled_messages.current_offset == consumed_offset {
                    trace!("No new messages to consume in partition ID: {partition_id}, topic: {topic_id}, stream: {stream_id}, consumer: {consumer}");
                    if auto_commit_enabled && stored_offset < consumed_offset {
                        trace!("Auto-committing the offset: {consumed_offset} in partition ID: {partition_id}, topic: {topic_id}, stream: {stream_id}, consumer: {consumer}");
                        client
                            .read()
                            .await
                            .store_consumer_offset(
                                &consumer,
                                &stream_id,
                                &topic_id,
                                Some(partition_id),
                                consumed_offset,
                            )
                            .await?;
                        if let Some(stored_offset_entry) = last_stored_offset.get(&partition_id) {
                            stored_offset_entry.store(consumed_offset, ORDERING);
                        } else {
                            last_stored_offset
                                .insert(partition_id, AtomicU64::new(consumed_offset));
                        }
                    }

                    return Ok(PolledMessages {
                        messages: EMPTY_MESSAGES,
                        current_offset: polled_messages.current_offset,
                        partition_id,
                    });
                }

                return Ok(polled_messages);
            }

            let error = polled_messages.unwrap_err();
            error!("Failed to poll messages: {error}");
            if matches!(error, IggyError::Disconnected | IggyError::Unauthenticated) {
                trace!("Retrying to poll messages in {retry_interval}...");
                sleep(retry_interval.get_duration()).await;
            }
            Err(error)
        }
    }

    async fn wait_before_polling(interval: u64, last_sent_at: u64) {
        if interval == 0 {
            return;
        }

        let now: u64 = IggyTimestamp::now().into();
        let elapsed = now - last_sent_at;
        if elapsed >= interval {
            trace!("No need to wait before polling messages. {now} - {last_sent_at} = {elapsed}");
            return;
        }

        let remaining = interval - elapsed;
        trace!("Waiting for {remaining} microseconds before polling messages... {interval} - {elapsed} = {remaining}");
        sleep(Duration::from_micros(remaining)).await;
    }

    async fn initialize_consumer_group(
        client: IggySharedMut<Box<dyn Client>>,
        create_consumer_group_if_not_exists: bool,
        stream_id: Arc<Identifier>,
        topic_id: Arc<Identifier>,
        consumer: Arc<Consumer>,
        consumer_name: &str,
        joined_consumer_group: Arc<AtomicBool>,
    ) -> Result<(), IggyError> {
        if joined_consumer_group.load(ORDERING) {
            return Ok(());
        }

        let client = client.read().await;
        let (name, id) = match consumer.id.kind {
            IdKind::Numeric => (consumer_name.to_owned(), Some(consumer.id.get_u32_value()?)),
            IdKind::String => (consumer.id.get_string_value()?, None),
        };

        let consumer_group_id = name.to_owned().try_into()?;
        trace!("Validating consumer group: {consumer_group_id} for topic: {topic_id}, stream: {stream_id}");
        if let Err(error) = client
            .get_consumer_group(&stream_id, &topic_id, &consumer_group_id)
            .await
        {
            if !create_consumer_group_if_not_exists {
                error!("Consumer group does not exist and auto-creation is disabled.");
                return Err(error);
            }

            info!("Creating consumer group: {consumer_group_id} for topic: {topic_id}, stream: {stream_id}");
            client
                .create_consumer_group(&stream_id, &topic_id, &name, id)
                .await?;
        }

        info!("Joining consumer group: {consumer_group_id} for topic: {topic_id}, stream: {stream_id}",);
        if let Err(error) = client
            .join_consumer_group(&stream_id, &topic_id, &consumer_group_id)
            .await
        {
            joined_consumer_group.store(false, ORDERING);
            error!("Failed to join consumer group: {consumer_group_id} for topic: {topic_id}, stream: {stream_id}: {error}");
            return Err(error);
        }

        joined_consumer_group.store(true, ORDERING);
        info!(
            "Joined consumer group: {consumer_group_id} for topic: {topic_id}, stream: {stream_id}"
        );
        Ok(())
    }
}

pub struct ReceivedMessage {
    pub message: PolledMessage,
    pub current_offset: u64,
    pub partition_id: u32,
}

impl ReceivedMessage {
    pub fn new(message: PolledMessage, current_offset: u64, partition_id: u32) -> Self {
        Self {
            message,
            current_offset,
            partition_id,
        }
    }
}

impl Stream for IggyConsumer {
    type Item = Result<ReceivedMessage, IggyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let partition_id = self.current_partition_id.load(ORDERING);
        if let Some(message) = self.buffered_messages.pop_front() {
            {
                if let Some(last_consumed_offset_entry) =
                    self.last_consumed_offsets.get(&partition_id)
                {
                    last_consumed_offset_entry.store(message.offset, ORDERING);
                } else {
                    self.last_consumed_offsets
                        .insert(partition_id, AtomicU64::new(message.offset));
                }

                if (self.store_after_every_nth_message > 0
                    && message.offset % self.store_after_every_nth_message == 0)
                    || self.store_offset_after_each_message
                {
                    self.store_offset(partition_id, message.offset);
                }
            }

            if self.buffered_messages.is_empty() {
                if self.polling_strategy.kind == PollingKind::Offset {
                    self.polling_strategy = PollingStrategy::offset(message.offset + 1);
                }

                if self.store_offset_after_all_messages {
                    self.store_offset(partition_id, message.offset);
                }
            }

            let current_offset;
            if let Some(current_offset_entry) = self.current_offsets.get(&partition_id) {
                current_offset = current_offset_entry.load(ORDERING);
            } else {
                current_offset = 0;
            }

            return Poll::Ready(Some(Ok(ReceivedMessage::new(
                message,
                current_offset,
                partition_id,
            ))));
        }

        if self.poll_future.is_none() {
            let future = self.create_poll_messages_future();
            self.poll_future = Some(Box::pin(future));
        }

        while let Some(future) = self.poll_future.as_mut() {
            match future.poll_unpin(cx) {
                Poll::Ready(Ok(mut polled_messages)) => {
                    let partition_id = polled_messages.partition_id;
                    self.current_partition_id.store(partition_id, ORDERING);
                    if polled_messages.messages.is_empty() {
                        self.poll_future = Some(Box::pin(self.create_poll_messages_future()));
                    } else {
                        if let Some(ref encryptor) = self.encryptor {
                            for message in &mut polled_messages.messages {
                                let payload = encryptor.decrypt(&message.payload);
                                if payload.is_err() {
                                    self.poll_future = None;
                                    error!("Failed to decrypt the message payload at offset: {}, partition ID: {}", message.offset, partition_id);
                                    let error = payload.unwrap_err();
                                    return Poll::Ready(Some(Err(error)));
                                }

                                let payload = payload.unwrap();
                                message.payload = Bytes::from(payload);
                                message.length = message.payload.len() as u32;
                            }
                        }

                        if let Some(current_offset_entry) = self.current_offsets.get(&partition_id)
                        {
                            current_offset_entry.store(polled_messages.current_offset, ORDERING);
                        } else {
                            self.current_offsets.insert(
                                partition_id,
                                AtomicU64::new(polled_messages.current_offset),
                            );
                        }

                        let message = polled_messages.messages.remove(0);
                        self.buffered_messages.extend(polled_messages.messages);

                        if self.polling_strategy.kind == PollingKind::Offset {
                            self.polling_strategy = PollingStrategy::offset(message.offset + 1);
                        }

                        if let Some(last_consumed_offset_entry) =
                            self.last_consumed_offsets.get(&partition_id)
                        {
                            last_consumed_offset_entry.store(message.offset, ORDERING);
                        } else {
                            self.last_consumed_offsets
                                .insert(partition_id, AtomicU64::new(message.offset));
                        }

                        if (self.store_after_every_nth_message > 0
                            && message.offset % self.store_after_every_nth_message == 0)
                            || self.store_offset_after_each_message
                            || (self.store_offset_after_all_messages
                                && self.buffered_messages.is_empty())
                        {
                            self.store_offset(polled_messages.partition_id, message.offset);
                        }

                        self.poll_future = None;
                        return Poll::Ready(Some(Ok(ReceivedMessage::new(
                            message,
                            polled_messages.current_offset,
                            polled_messages.partition_id,
                        ))));
                    }
                }
                Poll::Ready(Err(err)) => {
                    self.poll_future = None;
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Pending
    }
}

#[derive(Debug)]
pub struct IggyConsumerBuilder {
    client: IggySharedMut<Box<dyn Client>>,
    consumer_name: String,
    consumer: Consumer,
    stream: Identifier,
    topic: Identifier,
    partition: Option<u32>,
    polling_strategy: PollingStrategy,
    polling_interval: Option<IggyDuration>,
    batch_size: u32,
    auto_commit: AutoCommit,
    auto_join_consumer_group: bool,
    create_consumer_group_if_not_exists: bool,
    encryptor: Option<Arc<dyn Encryptor>>,
    retry_interval: IggyDuration,
}

impl IggyConsumerBuilder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: IggySharedMut<Box<dyn Client>>,
        consumer_name: String,
        consumer: Consumer,
        stream_id: Identifier,
        topic_id: Identifier,
        partition_id: Option<u32>,
        encryptor: Option<Arc<dyn Encryptor>>,
        polling_interval: Option<IggyDuration>,
    ) -> Self {
        Self {
            client,
            consumer_name,
            consumer,
            stream: stream_id,
            topic: topic_id,
            partition: partition_id,
            polling_strategy: PollingStrategy::next(),
            batch_size: 1000,
            auto_commit: AutoCommit::IntervalOrWhen(
                IggyDuration::ONE_SECOND,
                AutoCommitWhen::PollingMessages,
            ),
            auto_join_consumer_group: true,
            create_consumer_group_if_not_exists: true,
            encryptor,
            polling_interval,
            retry_interval: IggyDuration::ONE_SECOND,
        }
    }

    /// Sets the stream identifier.
    pub fn stream(self, stream: Identifier) -> Self {
        Self { stream, ..self }
    }

    /// Sets the topic identifier.
    pub fn topic(self, topic: Identifier) -> Self {
        Self { topic, ..self }
    }

    /// Sets the partition identifier.
    pub fn partition(self, partition: Option<u32>) -> Self {
        Self { partition, ..self }
    }

    /// Sets the polling strategy.
    pub fn polling_strategy(self, polling_strategy: PollingStrategy) -> Self {
        Self {
            polling_strategy,
            ..self
        }
    }

    /// Sets the batch size for polling messages.
    pub fn batch_size(self, batch_size: u32) -> Self {
        Self { batch_size, ..self }
    }

    /// Sets the auto-commit configuration for storing the offset on the server.
    pub fn auto_commit(self, auto_commit: AutoCommit) -> Self {
        Self {
            auto_commit,
            ..self
        }
    }

    /// Automatically joins the consumer group if the consumer is a part of a consumer group.
    pub fn auto_join_consumer_group(self) -> Self {
        Self {
            auto_join_consumer_group: true,
            ..self
        }
    }

    /// Does not automatically join the consumer group if the consumer is a part of a consumer group.
    pub fn do_not_auto_join_consumer_group(self) -> Self {
        Self {
            auto_join_consumer_group: false,
            ..self
        }
    }

    /// Automatically creates the consumer group if it does not exist.
    pub fn create_consumer_group_if_not_exists(self) -> Self {
        Self {
            create_consumer_group_if_not_exists: true,
            ..self
        }
    }

    /// Does not automatically create the consumer group if it does not exist.
    pub fn do_not_create_consumer_group_if_not_exists(self) -> Self {
        Self {
            create_consumer_group_if_not_exists: false,
            ..self
        }
    }

    /// Sets the polling interval for messages.
    pub fn poll_interval(self, interval: IggyDuration) -> Self {
        Self {
            polling_interval: Some(interval),
            ..self
        }
    }

    /// Clears the polling interval for messages.
    pub fn without_poll_interval(self) -> Self {
        Self {
            polling_interval: None,
            ..self
        }
    }

    /// Sets the encryptor for decrypting the messages' payloads.
    pub fn encryptor(self, encryptor: Arc<dyn Encryptor>) -> Self {
        Self {
            encryptor: Some(encryptor),
            ..self
        }
    }

    /// Clears the encryptor for decrypting the messages' payloads.
    pub fn without_encryptor(self) -> Self {
        Self {
            encryptor: None,
            ..self
        }
    }

    /// Sets the retry interval in case of server disconnection.
    pub fn retry_interval(self, interval: IggyDuration) -> Self {
        Self {
            retry_interval: interval,
            ..self
        }
    }

    pub fn build(self) -> IggyConsumer {
        IggyConsumer::new(
            self.client,
            self.consumer_name,
            self.consumer,
            self.stream,
            self.topic,
            self.partition,
            self.polling_interval,
            self.polling_strategy,
            self.batch_size,
            self.auto_commit,
            self.auto_join_consumer_group,
            self.create_consumer_group_if_not_exists,
            self.encryptor,
            self.retry_interval,
        )
    }
}