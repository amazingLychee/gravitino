/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *  http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */
use crate::config::AppConfig;
use crate::filesystem::{
    FileReader, FileStat, FileSystemCapacity, FileSystemContext, FileWriter, PathFileSystem, Result,
};
use crate::opened_file::{OpenFileFlags, OpenedFile};
use async_trait::async_trait;
use bytes::Bytes;
use fuse3::FileType::{Directory, RegularFile};
use fuse3::{Errno, FileType, Timestamp};
use log::error;
use opendal::{EntryMode, ErrorKind, Metadata, Operator};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) struct OpenDalFileSystem {
    op: Operator,
}

impl OpenDalFileSystem {}

impl OpenDalFileSystem {
    pub(crate) fn new(op: Operator, _config: &AppConfig, _fs_context: &FileSystemContext) -> Self {
        Self { op: op }
    }

    fn opendal_meta_to_file_stat(&self, meta: &Metadata, file_stat: &mut FileStat) {
        let now = SystemTime::now();
        let mtime = meta.last_modified().map(|x| x.into()).unwrap_or(now);

        file_stat.size = meta.content_length();
        file_stat.kind = opendal_filemode_to_filetype(meta.mode());
        file_stat.ctime = Timestamp::from(mtime);
        file_stat.atime = Timestamp::from(now);
        file_stat.mtime = Timestamp::from(mtime);
    }
}

#[async_trait]
impl PathFileSystem for OpenDalFileSystem {
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    async fn stat(&self, path: &Path) -> Result<FileStat> {
        let file_name = path.to_string_lossy().to_string();
        let meta_result = self.op.stat(&file_name).await;

        // path may be a directory, so try to stat it as a directory
        let meta = match meta_result {
            Ok(meta) => meta,
            Err(err) => {
                if err.kind() == ErrorKind::NotFound {
                    let dir_name = build_dir_path(path);
                    self.op
                        .stat(&dir_name)
                        .await
                        .map_err(opendal_error_to_errno)?
                } else {
                    return Err(opendal_error_to_errno(err));
                }
            }
        };

        let mut file_stat = FileStat::new_file_filestat_with_path(path, 0);
        self.opendal_meta_to_file_stat(&meta, &mut file_stat);

        Ok(file_stat)
    }

    async fn read_dir(&self, path: &Path) -> Result<Vec<FileStat>> {
        // dir name should end with '/' in opendal.
        let dir_name = build_dir_path(path);
        let entries = self
            .op
            .list(&dir_name)
            .await
            .map_err(opendal_error_to_errno)?;
        entries
            .iter()
            .map(|entry| {
                let mut path = PathBuf::from(path);
                path.push(entry.name());

                let mut file_stat = FileStat::new_file_filestat_with_path(&path, 0);
                self.opendal_meta_to_file_stat(entry.metadata(), &mut file_stat);
                Ok(file_stat)
            })
            .collect()
    }

    async fn open_file(&self, path: &Path, flags: OpenFileFlags) -> Result<OpenedFile> {
        let file_stat = self.stat(path).await?;
        debug_assert!(file_stat.kind == RegularFile);

        let mut file = OpenedFile::new(file_stat);
        let file_name = path.to_string_lossy().to_string();
        if flags.is_read() {
            let reader = self
                .op
                .reader_with(&file_name)
                .await
                .map_err(opendal_error_to_errno)?;
            file.reader = Some(Box::new(FileReaderImpl { reader }));
        }
        if flags.is_write() || flags.is_create() || flags.is_append() || flags.is_truncate() {
            let writer = self
                .op
                .writer_with(&file_name)
                .await
                .map_err(opendal_error_to_errno)?;
            file.writer = Some(Box::new(FileWriterImpl { writer }));
        }
        Ok(file)
    }

    async fn open_dir(&self, path: &Path, _flags: OpenFileFlags) -> Result<OpenedFile> {
        let file_stat = self.stat(path).await?;
        debug_assert!(file_stat.kind == Directory);

        let opened_file = OpenedFile::new(file_stat);
        Ok(opened_file)
    }

    async fn create_file(&self, path: &Path, flags: OpenFileFlags) -> Result<OpenedFile> {
        let file_name = path.to_string_lossy().to_string();

        let mut writer = self
            .op
            .writer_with(&file_name)
            .await
            .map_err(opendal_error_to_errno)?;

        writer.close().await.map_err(opendal_error_to_errno)?;

        let file = self.open_file(path, flags).await?;
        Ok(file)
    }

    async fn create_dir(&self, path: &Path) -> Result<FileStat> {
        let dir_name = build_dir_path(path);
        self.op
            .create_dir(&dir_name)
            .await
            .map_err(opendal_error_to_errno)?;
        let file_stat = self.stat(path).await?;
        Ok(file_stat)
    }

    async fn set_attr(&self, _path: &Path, _file_stat: &FileStat, _flush: bool) -> Result<()> {
        // no need to implement
        Ok(())
    }

    async fn remove_file(&self, path: &Path) -> Result<()> {
        let file_name = path.to_string_lossy().to_string();
        self.op
            .remove(vec![file_name])
            .await
            .map_err(opendal_error_to_errno)
    }

    async fn remove_dir(&self, path: &Path) -> Result<()> {
        //todo:: need to consider keeping the behavior of posix remove dir when the dir is not empty
        let dir_name = build_dir_path(path);
        self.op
            .remove(vec![dir_name])
            .await
            .map_err(opendal_error_to_errno)
    }

    fn get_capacity(&self) -> Result<FileSystemCapacity> {
        Ok(FileSystemCapacity {})
    }
}

struct FileReaderImpl {
    reader: opendal::Reader,
}

#[async_trait]
impl FileReader for FileReaderImpl {
    async fn read(&mut self, offset: u64, size: u32) -> Result<Bytes> {
        let end = offset + size as u64;
        let v = self
            .reader
            .read(offset..end)
            .await
            .map_err(opendal_error_to_errno)?;
        Ok(v.to_bytes())
    }
}

struct FileWriterImpl {
    writer: opendal::Writer,
}

#[async_trait]
impl FileWriter for FileWriterImpl {
    async fn write(&mut self, _offset: u64, data: &[u8]) -> Result<u32> {
        self.writer
            .write(data.to_vec())
            .await
            .map_err(opendal_error_to_errno)?;
        Ok(data.len() as u32)
    }

    async fn close(&mut self) -> Result<()> {
        self.writer.close().await.map_err(opendal_error_to_errno)?;
        Ok(())
    }
}

fn build_dir_path(path: &Path) -> String {
    let mut dir_path = path.to_string_lossy().to_string();
    if !dir_path.ends_with('/') {
        dir_path.push('/');
    }
    dir_path
}

fn opendal_error_to_errno(err: opendal::Error) -> Errno {
    error!("opendal operator error {:?}", err);
    match err.kind() {
        ErrorKind::Unsupported => Errno::from(libc::EOPNOTSUPP),
        ErrorKind::IsADirectory => Errno::from(libc::EISDIR),
        ErrorKind::NotFound => Errno::from(libc::ENOENT),
        ErrorKind::PermissionDenied => Errno::from(libc::EACCES),
        ErrorKind::AlreadyExists => Errno::from(libc::EEXIST),
        ErrorKind::NotADirectory => Errno::from(libc::ENOTDIR),
        ErrorKind::RateLimited => Errno::from(libc::EBUSY),
        _ => Errno::from(libc::ENOENT),
    }
}

fn opendal_filemode_to_filetype(mode: EntryMode) -> FileType {
    match mode {
        EntryMode::DIR => Directory,
        _ => RegularFile,
    }
}

#[cfg(test)]
mod test {
    use crate::config::AppConfig;
    use crate::s3_filesystem::extract_s3_config;
    use crate::s3_filesystem::tests::s3_test_config;
    use crate::test_enable_with;
    use crate::RUN_TEST_WITH_S3;
    use opendal::layers::LoggingLayer;
    use opendal::{services, Builder, Operator};

    fn create_opendal(config: &AppConfig) -> Operator {
        let opendal_config = extract_s3_config(config);
        let builder = services::S3::from_map(opendal_config);

        // Init an operator
        Operator::new(builder)
            .expect("opendal create failed")
            .layer(LoggingLayer::default())
            .finish()
    }

    #[tokio::test]
    async fn s3_ut_test_s3_stat() {
        test_enable_with!(RUN_TEST_WITH_S3);

        let config = s3_test_config();
        let op = create_opendal(&config);
        let path = "/";
        let list = op.list(path).await;
        if let Ok(l) = list {
            for i in l {
                println!("list result: {:?}", i);
            }
        } else {
            println!("list error: {:?}", list.err());
        }

        let meta = op.stat_with(path).await;
        if let Ok(m) = meta {
            println!("stat result: {:?}", m);
        } else {
            println!("stat error: {:?}", meta.err());
        }
    }

    #[tokio::test]
    async fn s3_ut_test_s3_delete() {
        test_enable_with!(RUN_TEST_WITH_S3);
        let config = s3_test_config();

        let op = create_opendal(&config);
        let path = "/s1/fileset1/gvfs_test/test_dir/test_file";

        let meta = op.stat(path).await;
        if let Ok(m) = meta {
            println!("stat result: {:?}", m);
        } else {
            println!("stat error: {:?}", meta.err());
        }

        let result = op.remove(vec![path.to_string()]).await;
        match result {
            Ok(_) => {
                println!("Delete successful (or no-op).");
            }
            Err(e) => {
                println!("Delete failed: {:?}", e);
            }
        }
    }
}
