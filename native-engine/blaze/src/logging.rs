// Copyright 2022 The Blaze Authors
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

use chrono::Local;
use log::{Level, LevelFilter, Log, Metadata, Record};

const MAX_LEVEL: Level = Level::Info;

pub fn init_logging() {
    log::set_logger(&SimpleLogger).expect("error setting logger");
    log::set_max_level(LevelFilter::Info);
}

#[derive(Clone, Copy)]
struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= MAX_LEVEL
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let local_time = Local::now().format("%d/%m/%Y %H:%M:%S");
            eprintln!(
                "{} [{}] Blaze - {}",
                local_time,
                record.level(),
                record.args()
            );
        }
    }

    fn flush(&self) {
        // do nothing
    }
}
