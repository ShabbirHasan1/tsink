//! Disk partition implementation.

use crate::encoding::GorillaDecoder;
use crate::label::{marshal_metric_name, unmarshal_metric_name};
use crate::mmap::PlatformMmap;
use crate::{DataPoint, Label, Result, Row, TsinkError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DATA_FILE_NAME: &str = "data";
pub const META_FILE_NAME: &str = "meta.json";

/// Metadata for a disk partition.
#[derive(Debug, Serialize, Deserialize)]
pub struct PartitionMeta {
    pub min_timestamp: i64,
    pub max_timestamp: i64,
    pub num_data_points: usize,
    pub metrics: HashMap<String, DiskMetric>,
    pub created_at: SystemTime,
}

/// Metadata for a metric in a disk partition.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiskMetric {
    pub name: String,
    pub offset: u64,
    pub min_timestamp: i64,
    pub max_timestamp: i64,
    pub num_data_points: usize,
}

/// A disk partition stores time-series data on disk using memory-mapped files.
pub struct DiskPartition {
    dir_path: PathBuf,
    meta: PartitionMeta,
    mapped_file: PlatformMmap,
    retention: Duration,
}

impl DiskPartition {
    /// Helper method to decode points from a disk metric.
    fn decode_metric_points(
        &self,
        disk_metric: &DiskMetric,
        start: i64,
        end: i64,
    ) -> Result<Vec<DataPoint>> {
        // Early exit if query range is completely outside metric range
        if end <= disk_metric.min_timestamp || start >= disk_metric.max_timestamp {
            return Ok(Vec::new());
        }

        // Validate offset is within bounds
        let offset = disk_metric.offset as usize;
        if offset >= self.mapped_file.len() {
            return Err(TsinkError::InvalidOffset {
                offset: disk_metric.offset,
                max: self.mapped_file.len() as u64,
            });
        }

        // Create a cursor at the metric's offset with bounds checking
        let data_slice = self.mapped_file.as_slice();
        let end_offset = std::cmp::min(
            data_slice.len(),
            offset + (disk_metric.num_data_points * 16),
        );
        let metric_data = &data_slice[offset..end_offset];
        let cursor = Cursor::new(metric_data.to_vec());

        // Decode points
        let mut decoder = GorillaDecoder::new(cursor.into_inner());
        let mut points = Vec::with_capacity(disk_metric.num_data_points);

        // Must decode all points sequentially due to delta encoding
        for _ in 0..disk_metric.num_data_points {
            let point = decoder.decode_point()?;

            if point.timestamp < start {
                continue;
            }
            if point.timestamp >= end {
                break;
            }

            points.push(point);
        }

        Ok(points)
    }

    /// Opens an existing disk partition.
    pub fn open(dir_path: impl AsRef<Path>, retention: Duration) -> Result<Self> {
        let dir_path = dir_path.as_ref();

        // Read metadata
        let meta_path = dir_path.join(META_FILE_NAME);
        if !meta_path.exists() {
            return Err(TsinkError::InvalidPartition {
                id: dir_path.to_string_lossy().to_string(),
            });
        }

        let meta_file = File::open(&meta_path)?;
        let meta: PartitionMeta = serde_json::from_reader(meta_file)?;

        // Memory-map the data file
        let data_path = dir_path.join(DATA_FILE_NAME);
        let data_file = File::open(&data_path)?;

        if data_file.metadata()?.len() == 0 {
            return Err(TsinkError::NoDataPoints {
                metric: "unknown".to_string(),
                start: 0,
                end: 0,
            });
        }

        let file_len = data_file.metadata()?.len() as usize;
        let mapped_file = PlatformMmap::new_readonly(data_file, file_len)?;

        Ok(Self {
            dir_path: dir_path.to_path_buf(),
            meta,
            mapped_file,
            retention,
        })
    }

    /// Creates a new disk partition from memory partition data.
    pub fn create(
        dir_path: impl AsRef<Path>,
        meta: PartitionMeta,
        data: Vec<u8>,
        retention: Duration,
    ) -> Result<Self> {
        let dir_path = dir_path.as_ref();

        // Create directory
        fs::create_dir_all(dir_path)?;

        // Write data file
        let data_path = dir_path.join(DATA_FILE_NAME);
        fs::write(&data_path, &data)?;

        // Write metadata file (write last to indicate valid partition)
        let meta_path = dir_path.join(META_FILE_NAME);
        let meta_file = File::create(&meta_path)?;
        serde_json::to_writer_pretty(meta_file, &meta)?;

        // Open the created partition
        Self::open(dir_path, retention)
    }
}

impl crate::partition::Partition for DiskPartition {
    fn insert_rows(&self, _rows: &[Row]) -> Result<Vec<Row>> {
        Err(TsinkError::ReadOnlyPartition {
            path: self.dir_path.clone(),
        })
    }

    fn select_data_points(
        &self,
        metric: &str,
        labels: &[Label],
        start: i64,
        end: i64,
    ) -> Result<Vec<DataPoint>> {
        if self.expired() {
            return Err(TsinkError::NoDataPoints {
                metric: "unknown".to_string(),
                start: 0,
                end: 0,
            });
        }

        let metric_name = marshal_metric_name(metric, labels);
        let metric_name_str = String::from_utf8_lossy(&metric_name);

        let disk_metric = match self.meta.metrics.get(metric_name_str.as_ref()) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };

        self.decode_metric_points(disk_metric, start, end)
    }

    fn select_all_labels(
        &self,
        metric: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<(Vec<Label>, Vec<DataPoint>)>> {
        if self.expired() {
            return Err(TsinkError::NoDataPoints {
                metric: metric.to_string(),
                start,
                end,
            });
        }

        let mut results = Vec::new();

        // Iterate through all metrics in metadata
        for (marshaled_name, disk_metric) in &self.meta.metrics {
            // Try to unmarshal the name to extract base metric and labels
            let marshaled_bytes = marshaled_name.as_bytes();

            // First try to unmarshal it as a marshaled name
            if let Ok((base_metric, labels)) = unmarshal_metric_name(marshaled_bytes) {
                if base_metric == metric {
                    // Found a matching metric, decode its data points
                    let points = self.decode_metric_points(disk_metric, start, end)?;
                    if !points.is_empty() {
                        results.push((labels, points));
                    }
                }
            } else if marshaled_name == metric {
                // It might be a plain metric name without labels
                let points = self.decode_metric_points(disk_metric, start, end)?;
                if !points.is_empty() {
                    results.push((Vec::new(), points));
                }
            }
        }

        Ok(results)
    }

    fn min_timestamp(&self) -> i64 {
        self.meta.min_timestamp
    }

    fn max_timestamp(&self) -> i64 {
        self.meta.max_timestamp
    }

    fn size(&self) -> usize {
        self.meta.num_data_points
    }

    fn active(&self) -> bool {
        false // Disk partitions are always read-only
    }

    fn expired(&self) -> bool {
        if let Ok(elapsed) = self.meta.created_at.elapsed() {
            elapsed > self.retention
        } else {
            false
        }
    }

    fn clean(&self) -> Result<()> {
        fs::remove_dir_all(&self.dir_path)?;
        Ok(())
    }

    fn flush_to_disk(&self) -> Result<Option<(Vec<u8>, PartitionMeta)>> {
        // DiskPartition is already on disk, so return None
        Ok(None)
    }
}
