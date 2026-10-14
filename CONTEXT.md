# Docker Pull Tar

This context describes the language used when selecting, downloading, verifying, and exporting container images for offline use.

## Language

**Image Reference**:
The registry, repository, and tag or digest that uniquely identify requested image content.
_Avoid_: Package name, image URL

**Registry**:
A service that publishes image manifests, configuration objects, and layers.
_Avoid_: Mirror, image source

**Mirror**:
A user-configured registry endpoint, display name, default status, and optional credential association used to resolve image references.
_Avoid_: Registry configuration, download source

**Platform**:
The operating system, architecture, and optional variant selected from a multi-platform image.
_Avoid_: Architecture when the operating system or variant also matters

**Download Task**:
One attempt to resolve an image, acquire and verify its content, and produce an archive.
_Avoid_: Job, pull process

**Snapshot**:
The complete current state of a download task, including its identity, phase, progress, and terminal outcome.
_Avoid_: Status response, progress message

**Layer**:
An ordered filesystem change set referenced by an image manifest and identified by its content digest.
_Avoid_: Blob when referring specifically to an image filesystem layer

**Resume Workspace**:
The retained partial content and metadata that allow an interrupted download task to continue safely.
_Avoid_: Cache, temporary files

**Docker Archive**:
The final tar file containing an image in a layout accepted by `docker load`.
_Avoid_: Image package, output bundle

**History Entry**:
The durable record of a completed, failed, or cancelled download task.
_Avoid_: Log entry, download item

**Download Batch**:
A user-submitted group of up to ten download tasks that share one registry and credential context and can run concurrently.
_Avoid_: Queue when referring to the submitted group itself

**Batch Task**:
One image download task within a download batch. It has its own lifecycle, cancellation, retry, snapshot, and output.
_Avoid_: Layer task when referring to the image-level unit
