use crate::storage::LayerBuilderError;
use crate::storage::StorageError;
use aws_sdk_s3::error::SdkError;
use snafu::Snafu;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("s3 storage backend definitions must specify a bucket name"))]
    BucketNotSpecified,
    #[snafu(display("failed to check for an object in s3 cache: {source}"))]
    Check {
        source: SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
    },
    #[snafu(display("failed to copy object in s3 cache: {source}"))]
    Copy {
        source: SdkError<aws_sdk_s3::operation::copy_object::CopyObjectError>,
    },
    #[snafu(display("failed to delete object in s3 cache: {source}"))]
    Delete {
        source: SdkError<aws_sdk_s3::operation::delete_object::DeleteObjectError>,
    },
    #[snafu(display("failed to deserialize manifest: {source}"))]
    Deserialize { source: serde_json::Error },
    #[snafu(display("failed to finish multipart upload to s3 cache: {source}"))]
    Finish {
        source: SdkError<
            aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadError,
        >,
    },
    #[snafu(display("failed to get segment of an object in s3 cache: {source}"))]
    Get {
        source: SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    },
    #[snafu(display("failed to make a layer: {source}"))]
    Layer { source: LayerBuilderError },
    #[snafu(display("cannot save an artifact that is missing a layer with digest '{digest}'"))]
    LayerMissing { digest: String },
    #[snafu(display("failed to list objects in bucket: {source}"))]
    List {
        source: SdkError<aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error>,
    },
    #[snafu(display("storage backend does not contain an artifact with id: {id}"))]
    NotFound { id: crate::storage::Id },
    #[snafu(display("failed to upload part of a multipart upload to s3 cache: {source}"))]
    Part {
        source: SdkError<aws_sdk_s3::operation::upload_part::UploadPartError>,
    },
    #[snafu(display("failed to upload object to s3 cache: {source}"))]
    Put {
        source: SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
    },
    #[snafu(display("due to the danger of it we do not support prune-all on s3 backends, if you need to clear the bucket use the s3 console"))]
    PruneAll,
    #[snafu(display("failed to serialize manifest: {source}"))]
    Serialize { source: serde_json::Error },
    #[snafu(display("failed to start multipart upload to s3 cache: {source}"))]
    Start {
        source:
            SdkError<aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadError>,
    },
    #[snafu(display("start multipart upload to s3 cache failed to return an upload_id"))]
    StartNoID,
    #[snafu(display("failed to operate with temporary file for layer writing: {source}"))]
    Temp { source: std::io::Error },
}

impl From<Error> for StorageError {
    fn from(value: Error) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}
