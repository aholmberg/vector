use std::task::{Context, Poll};

use bytes::Bytes;
use rdkafka::{
    error::KafkaError,
    message::OwnedHeaders,
    producer::{FutureProducer, FutureRecord},
    util::Timeout,
};
use vector_core::internal_event::{
    ByteSize, BytesSent, InternalEventHandle as _, Protocol, Registered,
};
use vrl::value::Value;

use crate::{kafka::KafkaStatisticsContext, sinks::prelude::*};

pub struct KafkaRequest {
    pub body: Bytes,
    pub metadata: KafkaRequestMetadata,
    pub request_metadata: RequestMetadata,
}

pub struct KafkaRequestMetadata {
    pub finalizers: EventFinalizers,
    pub key: Option<Bytes>,
    pub timestamp_millis: Option<i64>,
    pub headers: Option<OwnedHeaders>,
    pub topic: String,
}

pub struct KafkaResponse {
    event_byte_size: GroupedCountByteSize,
}

impl DriverResponse for KafkaResponse {
    fn event_status(&self) -> EventStatus {
        EventStatus::Delivered
    }

    fn events_sent(&self) -> &GroupedCountByteSize {
        &self.event_byte_size
    }
}

impl Finalizable for KafkaRequest {
    fn take_finalizers(&mut self) -> EventFinalizers {
        std::mem::take(&mut self.metadata.finalizers)
    }
}

impl MetaDescriptive for KafkaRequest {
    fn get_metadata(&self) -> &RequestMetadata {
        &self.request_metadata
    }

    fn metadata_mut(&mut self) -> &mut RequestMetadata {
        &mut self.request_metadata
    }
}

#[derive(Clone)]
pub struct KafkaService {
    kafka_producer: FutureProducer<KafkaStatisticsContext>,
    bytes_sent: Registered<BytesSent>,
}

impl KafkaService {
    pub(crate) fn new(kafka_producer: FutureProducer<KafkaStatisticsContext>) -> KafkaService {
        KafkaService {
            kafka_producer,
            bytes_sent: register!(BytesSent::from(Protocol("kafka".into()))),
        }
    }
}

impl UserLoggingResponse for KafkaResponse {}
impl UserLoggingError for KafkaError {
    fn log_msg(&self) -> Option<Value> {
        match self {
            Self::ClientConfig(_, _, _, _) => Some("Invalid client configuration".into()),
            Self::ClientCreation(_) => Some("Client creation failed".into()),
            Self::ConsumerCommit(code) => {
                Some(format!("Consumer commit failed, code={code}").into())
            }
            Self::Flush(code) => Some(format!("Flushing failed, code={code}").into()),
            Self::Global(code) => Some(format!("Global error, code={code}").into()),
            Self::MessageProduction(code) => {
                Some(format!("Message production error, code={code}").into())
            }
            Self::StoreOffset(code) => Some(format!("Offset store failed, code={code}").into()),
            _ => None,
        }
    }
}

impl Service<KafkaRequest> for KafkaService {
    type Response = KafkaResponse;
    type Error = KafkaError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: KafkaRequest) -> Self::Future {
        let this = self.clone();

        Box::pin(async move {
            let event_byte_size = request
                .request_metadata
                .into_events_estimated_json_encoded_byte_size();

            let mut record =
                FutureRecord::to(&request.metadata.topic).payload(request.body.as_ref());
            if let Some(key) = &request.metadata.key {
                record = record.key(&key[..]);
            }
            if let Some(timestamp) = request.metadata.timestamp_millis {
                record = record.timestamp(timestamp);
            }
            if let Some(headers) = request.metadata.headers {
                record = record.headers(headers);
            }

            // rdkafka will internally retry forever if the queue is full
            match this.kafka_producer.send(record, Timeout::Never).await {
                Ok((_partition, _offset)) => {
                    this.bytes_sent.emit(ByteSize(
                        request.body.len() + request.metadata.key.map(|x| x.len()).unwrap_or(0),
                    ));
                    Ok(KafkaResponse { event_byte_size })
                }
                Err((kafka_err, _original_record)) => Err(kafka_err),
            }
        })
    }
}
