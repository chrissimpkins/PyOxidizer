// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
Functionality for interacting with Python.

This module tree holds functionality that is centered around Python.
*/

pub mod binary;
pub mod bytecode;
pub mod config;
pub mod distribution;
pub mod distutils;
pub mod embedded_resource;
pub mod filtering;
pub mod fsscan;
pub mod libpython;
pub mod package_metadata;
pub mod packaging_tool;
pub mod pyembed;
pub mod resource;
pub mod resources_policy;
pub mod standalone_distribution;
pub mod windows_embeddable_distribution;
