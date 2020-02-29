// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
Management of Python resources.
*/

use {
    byteorder::{LittleEndian, ReadBytesExt},
    python3_sys as pyffi,
    std::collections::{HashMap, HashSet},
    std::convert::TryFrom,
    std::ffi::CStr,
    std::io::{Cursor, Read},
    std::sync::Arc,
};

/// Header value for version 1 of resources payload.
const RESOURCES_HEADER_V1: &[u8] = b"pyembed\x01";

const FIELD_END_OF_INDEX: u8 = 0x00;
const FIELD_START_OF_ENTRY: u8 = 0x01;
const FIELD_END_OF_ENTRY: u8 = 0x02;
const FIELD_MODULE_NAME: u8 = 0x03;
const FIELD_IS_PACKAGE: u8 = 0x04;
const FIELD_IS_NAMESPACE_PACKAGE: u8 = 0x05;
const FIELD_IN_MEMORY_SOURCE: u8 = 0x06;
const FIELD_IN_MEMORY_BYTECODE: u8 = 0x07;
const FIELD_IN_MEMORY_BYTECODE_OPT1: u8 = 0x08;
const FIELD_IN_MEMORY_BYTECODE_OPT2: u8 = 0x09;
const FIELD_IN_MEMORY_EXTENSION_MODULE_SHARED_LIBRARY: u8 = 0x0a;
const FIELD_IN_MEMORY_RESOURCES_DATA: u8 = 0x0b;
const FIELD_IN_MEMORY_PACKAGE_DISTRIBUTION: u8 = 0x0c;
const FIELD_IN_MEMORY_SHARED_LIBRARY: u8 = 0x0d;
const FIELD_SHARED_LIBRARY_DEPENDENCY_NAMES: u8 = 0x0e;

/// Represents a Python module and all its metadata.
///
/// This holds the result of parsing an embedded resources data structure as well
/// as extra state to support importing frozen and builtin modules.
#[derive(Debug, PartialEq)]
pub(crate) struct EmbeddedResource<'a> {
    /// The resource name.
    pub name: &'a str,

    /// Whether the resource is a Python package.
    pub is_package: bool,

    /// Whether the resource is a Python namespace package.
    pub is_namespace_package: bool,

    /// Whether the resource is a builtin extension module in the Python interpreter.
    pub is_builtin: bool,

    /// Whether the resource is frozen into the Python interpreter.
    pub is_frozen: bool,

    /// In-memory source code for Python module.
    pub in_memory_source: Option<&'a [u8]>,

    /// In-memory bytecode for Python module.
    pub in_memory_bytecode: Option<&'a [u8]>,

    /// In-memory bytecode optimization level 1 for Python module.
    pub in_memory_bytecode_opt1: Option<&'a [u8]>,

    /// In-memory bytecode optimization level 2 for Python module.
    pub in_memory_bytecode_opt2: Option<&'a [u8]>,

    /// In-memory content of shared library providing Python module.
    pub in_memory_shared_library_extension_module: Option<&'a [u8]>,

    /// Resource "files" in this Python package.
    pub in_memory_resources: Option<Arc<Box<HashMap<&'a str, &'a [u8]>>>>,

    /// Python package distribution files.
    pub in_memory_package_distribution: Option<HashMap<&'a str, &'a [u8]>>,

    /// In-memory content of shared library to be loaded from memory.
    pub in_memory_shared_library: Option<&'a [u8]>,

    /// Names of shared libraries this entry depends on.
    pub shared_library_dependency_names: Option<Vec<&'a str>>,
}

impl<'a> Default for EmbeddedResource<'a> {
    fn default() -> Self {
        Self {
            name: "",
            is_package: false,
            is_namespace_package: false,
            is_builtin: false,
            is_frozen: false,
            in_memory_source: None,
            in_memory_bytecode: None,
            in_memory_bytecode_opt1: None,
            in_memory_bytecode_opt2: None,
            in_memory_shared_library_extension_module: None,
            in_memory_resources: None,
            in_memory_package_distribution: None,
            in_memory_shared_library: None,
            shared_library_dependency_names: None,
        }
    }
}

impl<'a> EmbeddedResource<'a> {
    /// Whether the module is imported by the importer provided by this crate.
    ///
    /// This excludes builtin and frozen modules, which are merely registered.
    pub(crate) fn uses_pyembed_importer(&self) -> bool {
        self.in_memory_bytecode.is_some()
            || self.in_memory_bytecode_opt1.is_some()
            || self.in_memory_bytecode_opt2.is_some()
            || self.in_memory_shared_library_extension_module.is_some()
    }
}

/// Defines Python resources available for import.
#[derive(Debug)]
pub(crate) struct PythonImporterState<'a> {
    /// Names of Python packages.
    pub packages: HashSet<&'static str>,

    pub resources: HashMap<&'a str, EmbeddedResource<'a>>,
}

impl<'a> Default for PythonImporterState<'a> {
    fn default() -> Self {
        Self {
            packages: HashSet::new(),
            resources: HashMap::new(),
        }
    }
}

impl<'a> PythonImporterState<'a> {
    /// Load state from the environment and by parsing data structures.
    pub fn load(&mut self, resources_data: &'static [u8]) -> Result<(), &'static str> {
        // Loading of builtin and frozen knows to mutate existing entries rather
        // than replace. So do these last.
        self.load_resources(resources_data)?;
        self.load_interpreter_builtin_modules()?;
        self.load_interpreter_frozen_modules()?;

        Ok(())
    }

    /// Load `builtin` modules from the Python interpreter.
    fn load_interpreter_builtin_modules(&mut self) -> Result<(), &'static str> {
        for i in 0.. {
            let record = unsafe { pyffi::PyImport_Inittab.offset(i) };

            if unsafe { *record }.name.is_null() {
                break;
            }

            let name = unsafe { CStr::from_ptr((*record).name as _) };
            let name_str = match name.to_str() {
                Ok(v) => v,
                Err(_) => {
                    return Err("unable to parse PyImport_Inittab");
                }
            };

            // Module can be defined by embedded resources data. If exists, just
            // update the big.
            if let Some(mut entry) = self.resources.get_mut(name_str) {
                entry.is_builtin = true;
            } else {
                self.resources.insert(
                    name_str,
                    EmbeddedResource {
                        name: name_str,
                        is_builtin: true,
                        ..EmbeddedResource::default()
                    },
                );
            }
        }

        Ok(())
    }

    /// Load `frozen` modules from the Python interpreter.
    fn load_interpreter_frozen_modules(&mut self) -> Result<(), &'static str> {
        for i in 0.. {
            let record = unsafe { pyffi::PyImport_FrozenModules.offset(i) };

            if unsafe { *record }.name.is_null() {
                break;
            }

            let name = unsafe { CStr::from_ptr((*record).name as _) };
            let name_str = match name.to_str() {
                Ok(v) => v,
                Err(_) => {
                    return Err("unable to parse PyImport_FrozenModules");
                }
            };

            // Module can be defined by embedded resources data. If exists, just
            // update the big.
            if let Some(mut entry) = self.resources.get_mut(name_str) {
                entry.is_frozen = true;
            } else {
                self.resources.insert(
                    name_str,
                    EmbeddedResource {
                        name: name_str,
                        is_frozen: true,
                        ..EmbeddedResource::default()
                    },
                );
            }
        }

        Ok(())
    }

    /// Load resources by parsing a blob.
    fn load_resources(&mut self, data: &'a [u8]) -> Result<(), &'static str> {
        let mut reader = Cursor::new(data);

        let mut header = [0; 8];
        reader
            .read_exact(&mut header)
            .or_else(|_| Err("error reading 8 byte header"))?;

        if header == RESOURCES_HEADER_V1 {
            self.load_resources_v1(data, &mut reader)
        } else {
            Err("unrecognized file format")
        }
    }

    fn load_resources_v1(
        &mut self,
        data: &'a [u8],
        reader: &mut Cursor<&[u8]>,
    ) -> Result<(), &'static str> {
        let blob_section_count = reader
            .read_u8()
            .or_else(|_| Err("failed reading blob section count"))?;
        let blob_index_length = reader
            .read_u32::<LittleEndian>()
            .or_else(|_| Err("failed reading blob index length"))?
            as usize;
        let resources_count = reader
            .read_u32::<LittleEndian>()
            .or_else(|_| Err("failed reading resources count"))?
            as usize;
        let resources_index_length = reader
            .read_u32::<LittleEndian>()
            .or_else(|_| Err("failed reading resources index length"))?
            as usize;

        // Now we have a series of (u8, u64) denoting the lengths of blob fields.
        // It is terminated by an END_OF_INDEX field.
        let mut total_blob_offset: usize = 0;
        let mut resource_name_blob_start_offset: usize = 0;
        let mut in_memory_source_blob_start_offset: usize = 0;
        let mut in_memory_bytecode_blob_start_offset: usize = 0;
        let mut in_memory_bytecode_opt1_blob_start_offset: usize = 0;
        let mut in_memory_bytecode_opt2_blob_start_offset: usize = 0;
        let mut in_memory_extension_module_shared_library_start_offset: usize = 0;
        let mut in_memory_resources_start_offset: usize = 0;
        let mut in_memory_package_distribution_offset: usize = 0;
        let mut in_memory_shared_library_start_offset: usize = 0;
        let mut shared_library_dependency_names_start_offset: usize = 0;

        if blob_index_length > 0 {
            for _ in 0..blob_section_count {
                let field = reader
                    .read_u8()
                    .or_else(|_| Err("failed reading blob length field type"))?;

                if field == FIELD_END_OF_INDEX {
                    return Err("unexpected end of blob index");
                }

                let blob_length = reader
                    .read_u64::<LittleEndian>()
                    .or_else(|_| Err("failed reading field blob length"))?;
                let blob_length = usize::try_from(blob_length)
                    .or_else(|_| Err("failed to convert blob size to usize"))?;

                if field == FIELD_MODULE_NAME {
                    resource_name_blob_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_SOURCE {
                    in_memory_source_blob_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_BYTECODE {
                    in_memory_bytecode_blob_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_BYTECODE_OPT1 {
                    in_memory_bytecode_opt1_blob_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_BYTECODE_OPT2 {
                    in_memory_bytecode_opt2_blob_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_EXTENSION_MODULE_SHARED_LIBRARY {
                    in_memory_extension_module_shared_library_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_RESOURCES_DATA {
                    in_memory_resources_start_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_PACKAGE_DISTRIBUTION {
                    in_memory_package_distribution_offset = total_blob_offset;
                } else if field == FIELD_IN_MEMORY_SHARED_LIBRARY {
                    in_memory_shared_library_start_offset = total_blob_offset;
                } else if field == FIELD_SHARED_LIBRARY_DEPENDENCY_NAMES {
                    shared_library_dependency_names_start_offset = total_blob_offset;
                } else {
                    return Err("unhandled field in blob length index");
                }

                total_blob_offset += blob_length;
            }

            let field = reader
                .read_u8()
                .or_else(|_| Err("failed to read end of blob index field"))?;
            if field != FIELD_END_OF_INDEX {
                return Err("unexpected value at end of blob index");
            }
        }

        let blob_start_offset: usize =
            // Magic.
            RESOURCES_HEADER_V1.len()
            // Global header.
            + 1 + 4 + 4 + 4
            + blob_index_length
            + resources_index_length
        ;

        let mut current_resource_name_offset = blob_start_offset + resource_name_blob_start_offset;
        let mut current_in_memory_source_offset =
            blob_start_offset + in_memory_source_blob_start_offset;
        let mut current_in_memory_bytecode_offset =
            blob_start_offset + in_memory_bytecode_blob_start_offset;
        let mut current_in_memory_bytecode_opt1_offset =
            blob_start_offset + in_memory_bytecode_opt1_blob_start_offset;
        let mut current_in_memory_bytecode_opt2_offset =
            blob_start_offset + in_memory_bytecode_opt2_blob_start_offset;
        let mut current_in_memory_extension_module_shared_library_offset =
            blob_start_offset + in_memory_extension_module_shared_library_start_offset;
        let mut current_in_memory_resources_offset =
            blob_start_offset + in_memory_resources_start_offset;
        let mut current_in_memory_package_distribution_offset =
            blob_start_offset + in_memory_package_distribution_offset;
        let mut current_in_memory_shared_library_offset =
            blob_start_offset + in_memory_shared_library_start_offset;
        let mut current_shared_library_dependency_names_offset =
            blob_start_offset + shared_library_dependency_names_start_offset;

        let mut current_resource = EmbeddedResource::default();
        let mut current_resource_name = None;
        let mut index_entry_count = 0;

        if resources_index_length == 0 || resources_count == 0 {
            return Ok(());
        }

        loop {
            let field_type = reader
                .read_u8()
                .or_else(|_| Err("failed reading field type"))?;

            match field_type {
                FIELD_END_OF_INDEX => break,
                FIELD_START_OF_ENTRY => {
                    index_entry_count += 1;
                    current_resource = EmbeddedResource::default();
                    current_resource_name = None;
                }

                FIELD_END_OF_ENTRY => {
                    if let Some(name) = current_resource_name {
                        self.resources.insert(name, current_resource);
                    } else {
                        return Err("resource name field is required");
                    }

                    current_resource = EmbeddedResource::default();
                    current_resource_name = None;
                }
                FIELD_MODULE_NAME => {
                    let l = reader
                        .read_u16::<LittleEndian>()
                        .or_else(|_| Err("failed reading resource name length"))?
                        as usize;

                    let name = unsafe {
                        std::str::from_utf8_unchecked(
                            &data[current_resource_name_offset..current_resource_name_offset + l],
                        )
                    };

                    current_resource_name = Some(name);
                    current_resource_name_offset += l;

                    current_resource.name = name;
                }
                FIELD_IS_PACKAGE => {
                    current_resource.is_package = true;
                }
                FIELD_IS_NAMESPACE_PACKAGE => {
                    current_resource.is_namespace_package = true;
                }
                FIELD_IN_MEMORY_SOURCE => {
                    let l = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading source length"))?
                        as usize;

                    current_resource.in_memory_source = Some(
                        &data[current_in_memory_source_offset..current_in_memory_source_offset + l],
                    );
                    current_in_memory_source_offset += l;
                }
                FIELD_IN_MEMORY_BYTECODE => {
                    let l = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading bytecode length"))?
                        as usize;

                    current_resource.in_memory_bytecode = Some(
                        &data[current_in_memory_bytecode_offset
                            ..current_in_memory_bytecode_offset + l],
                    );
                    current_in_memory_bytecode_offset += l;
                }
                FIELD_IN_MEMORY_BYTECODE_OPT1 => {
                    let l = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading bytecode length"))?
                        as usize;

                    current_resource.in_memory_bytecode_opt1 = Some(
                        &data[current_in_memory_bytecode_opt1_offset
                            ..current_in_memory_bytecode_opt1_offset + l],
                    );
                    current_in_memory_bytecode_opt1_offset += l;
                }
                FIELD_IN_MEMORY_BYTECODE_OPT2 => {
                    let l = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading bytecode length"))?
                        as usize;

                    current_resource.in_memory_bytecode_opt2 = Some(
                        &data[current_in_memory_bytecode_opt2_offset
                            ..current_in_memory_bytecode_opt2_offset + l],
                    );
                    current_in_memory_bytecode_opt2_offset += l;
                }
                FIELD_IN_MEMORY_EXTENSION_MODULE_SHARED_LIBRARY => {
                    let l = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading extension module length"))?
                        as usize;

                    current_resource.in_memory_shared_library_extension_module = Some(
                        &data[current_in_memory_extension_module_shared_library_offset
                            ..current_in_memory_extension_module_shared_library_offset + l],
                    );
                    current_in_memory_extension_module_shared_library_offset += l;
                }

                FIELD_IN_MEMORY_RESOURCES_DATA => {
                    let resource_count = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading resources length"))?
                        as usize;

                    let mut resources = Box::new(HashMap::with_capacity(resource_count));

                    for _ in 0..resource_count {
                        let resource_name_length = reader
                            .read_u16::<LittleEndian>()
                            .or_else(|_| Err("failed reading resource name"))?
                            as usize;

                        let resource_name = unsafe {
                            std::str::from_utf8_unchecked(
                                &data[current_in_memory_resources_offset
                                    ..current_in_memory_resources_offset + resource_name_length],
                            )
                        };
                        current_in_memory_resources_offset += resource_name_length;

                        let resource_length = reader
                            .read_u64::<LittleEndian>()
                            .or_else(|_| Err("failed reading resource length"))?
                            as usize;

                        let resource_data = &data[current_in_memory_resources_offset
                            ..current_in_memory_resources_offset + resource_length];
                        current_in_memory_resources_offset += resource_length;

                        resources.insert(resource_name, resource_data);
                    }

                    current_resource.in_memory_resources = Some(Arc::new(resources));
                }

                FIELD_IN_MEMORY_PACKAGE_DISTRIBUTION => {
                    let resource_count = reader
                        .read_u32::<LittleEndian>()
                        .or_else(|_| Err("failed reading package distribution length"))?
                        as usize;

                    let mut resources = HashMap::with_capacity(resource_count);

                    for _ in 0..resource_count {
                        let name_length = reader
                            .read_u16::<LittleEndian>()
                            .or_else(|_| Err("failed reading distribution metadata name"))?
                            as usize;

                        let name = unsafe {
                            std::str::from_utf8_unchecked(
                                &data[current_in_memory_package_distribution_offset
                                    ..current_in_memory_package_distribution_offset + name_length],
                            )
                        };
                        current_in_memory_package_distribution_offset += name_length;

                        let resource_length = reader.read_u64::<LittleEndian>().or_else(|_| {
                            Err("failed reading package distribution resource length")
                        })? as usize;

                        let resource_data = &data[current_in_memory_package_distribution_offset
                            ..current_in_memory_package_distribution_offset + resource_length];
                        current_in_memory_package_distribution_offset += resource_length;

                        resources.insert(name, resource_data);
                    }

                    current_resource.in_memory_package_distribution = Some(resources);
                }

                FIELD_IN_MEMORY_SHARED_LIBRARY => {
                    let l = reader
                        .read_u64::<LittleEndian>()
                        .or_else(|_| Err("failed reading in-memory shared library length"))?
                        as usize;

                    current_resource.in_memory_shared_library = Some(
                        &data[current_in_memory_shared_library_offset
                            ..current_in_memory_shared_library_offset + l],
                    );
                    current_in_memory_shared_library_offset += l;
                }

                FIELD_SHARED_LIBRARY_DEPENDENCY_NAMES => {
                    let names_count = reader
                        .read_u16::<LittleEndian>()
                        .or_else(|_| Err("failed reading shared library dependency names length"))?
                        as usize;

                    let mut names = Vec::new();

                    for _ in 0..names_count {
                        let name_length = reader.read_u16::<LittleEndian>().or_else(|_| {
                            Err("failed reading shared library dependency name length")
                        })? as usize;

                        let name = unsafe {
                            std::str::from_utf8_unchecked(
                                &data[current_shared_library_dependency_names_offset
                                    ..current_shared_library_dependency_names_offset + name_length],
                            )
                        };
                        current_shared_library_dependency_names_offset += name_length;

                        names.push(name);
                    }

                    current_resource.shared_library_dependency_names = Some(names);
                }

                _ => return Err("invalid field type"),
            }
        }

        if index_entry_count != resources_count {
            return Err("mismatch between advertised index count and actual");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        pyoxidizerlib::py_packaging::embedded_resource::{
            write_embedded_resources_v1, EmbeddedResource as OwnedEmbeddedResource,
        },
        std::collections::BTreeMap,
    };

    #[test]
    fn test_too_short_header() {
        let data = b"foo";

        let mut state = PythonImporterState::default();
        let res = state.load_resources(data);
        assert_eq!(res.err(), Some("error reading 8 byte header"));
    }

    #[test]
    fn test_unrecognized_header() {
        let data = b"pyembed\x00";
        let mut state = PythonImporterState::default();
        let res = state.load_resources(data);
        assert_eq!(res.err(), Some("unrecognized file format"));

        let data = b"pyembed\x02";
        let mut state = PythonImporterState::default();
        let res = state.load_resources(data);
        assert_eq!(res.err(), Some("unrecognized file format"));
    }

    #[test]
    fn test_no_indices() {
        let data = b"pyembed\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let mut state = PythonImporterState::default();
        state.load_resources(data).unwrap();
    }

    #[test]
    fn test_no_blob_index() {
        let data = b"pyembed\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00";
        let mut state = PythonImporterState::default();
        state.load_resources(data).unwrap();
    }

    #[test]
    fn test_no_resource_index() {
        let data = b"pyembed\x01\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let mut state = PythonImporterState::default();
        state.load_resources(data).unwrap();
    }

    #[test]
    fn test_empty_indices() {
        let data = b"pyembed\x01\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00";
        let mut state = PythonImporterState::default();
        state.load_resources(data).unwrap();
    }

    #[test]
    fn test_index_count_mismatch() {
        let data = b"pyembed\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x01\x00\x00\x00\x00";
        let mut state = PythonImporterState::default();
        let res = state.load_resources(data);
        assert_eq!(
            res.err(),
            Some("mismatch between advertised index count and actual")
        );
    }

    #[test]
    fn test_missing_resource_name() {
        let data =
            b"pyembed\x01\x00\x01\x00\x00\x00\x01\x00\x00\x00\x03\x00\x00\x00\x00\x01\x02\x00";
        let mut state = PythonImporterState::default();
        let res = state.load_resources(data);
        assert_eq!(res.err(), Some("resource name field is required"));
    }

    #[test]
    fn test_just_resource_name() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();

        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();
        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_multiple_resources_just_names() {
        let resource1 = OwnedEmbeddedResource {
            name: "foo".to_string(),
            ..OwnedEmbeddedResource::default()
        };

        let resource2 = OwnedEmbeddedResource {
            name: "module2".to_string(),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource1, resource2], &mut data).unwrap();

        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 2);

        let entry = state.resources.get("foo").unwrap();
        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                ..EmbeddedResource::default()
            }
        );

        let entry = state.resources.get("module2").unwrap();
        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "module2",
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_source() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_source: Some(b"source".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(entry.in_memory_source.unwrap(), b"source");

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_source: Some(&data[data.len() - 6..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_bytecode() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_bytecode: Some(b"bytecode".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(entry.in_memory_bytecode.unwrap(), b"bytecode");

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_bytecode: Some(&data[data.len() - 8..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_bytecode_opt1() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_bytecode_opt1: Some(b"bytecode".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(entry.in_memory_bytecode_opt1.unwrap(), b"bytecode");

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_bytecode_opt1: Some(&data[data.len() - 8..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_bytecode_opt2() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_bytecode_opt2: Some(b"bytecode".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(entry.in_memory_bytecode_opt2.unwrap(), b"bytecode");

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_bytecode_opt2: Some(&data[data.len() - 8..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_extension_module_shared_library() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_extension_module_shared_library: Some(b"em".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(
            entry.in_memory_shared_library_extension_module.unwrap(),
            b"em"
        );

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_shared_library_extension_module: Some(&data[data.len() - 2..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_in_memory_resources_data() {
        let mut resources = BTreeMap::new();
        resources.insert("foo".to_string(), b"foovalue".to_vec());
        resources.insert("another".to_string(), b"value2".to_vec());

        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_resources: Some(resources),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        let resources = entry.in_memory_resources.as_ref().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources.get("foo").unwrap(), b"foovalue");
        assert_eq!(resources.get("another").unwrap(), b"value2");
    }

    #[test]
    fn test_in_memory_package_distribution() {
        let mut resources = BTreeMap::new();
        resources.insert("foo".to_string(), b"foovalue".to_vec());
        resources.insert("another".to_string(), b"value2".to_vec());

        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_package_distribution: Some(resources),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        let resources = entry.in_memory_package_distribution.as_ref().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources.get("foo").unwrap(), b"foovalue");
        assert_eq!(resources.get("another").unwrap(), b"value2");
    }

    #[test]
    fn test_in_memory_shared_library() {
        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            in_memory_shared_library: Some(b"library".to_vec()),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(entry.in_memory_shared_library.unwrap(), b"library");

        assert_eq!(
            entry,
            &EmbeddedResource {
                name: "foo",
                in_memory_shared_library: Some(&data[data.len() - 7..data.len()]),
                ..EmbeddedResource::default()
            }
        );
    }

    #[test]
    fn test_shared_library_dependency_names() {
        let names = vec!["depends".to_string(), "libfoo".to_string()];

        let resource = OwnedEmbeddedResource {
            name: "foo".to_string(),
            shared_library_dependency_names: Some(names),
            ..OwnedEmbeddedResource::default()
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("foo").unwrap();

        assert_eq!(
            entry.shared_library_dependency_names,
            Some(vec!["depends", "libfoo"])
        );
    }

    #[test]
    fn test_all_fields() {
        let mut resources = BTreeMap::new();
        resources.insert("foo".to_string(), b"foovalue".to_vec());
        resources.insert("resource2".to_string(), b"value2".to_vec());

        let mut distribution = BTreeMap::new();
        distribution.insert("dist".to_string(), b"distvalue".to_vec());
        distribution.insert("dist2".to_string(), b"dist2value".to_vec());

        let resource = OwnedEmbeddedResource {
            name: "module".to_string(),
            is_package: true,
            is_namespace_package: true,
            in_memory_source: Some(b"source".to_vec()),
            in_memory_bytecode: Some(b"bytecode".to_vec()),
            in_memory_bytecode_opt1: Some(b"bytecodeopt1".to_vec()),
            in_memory_bytecode_opt2: Some(b"bytecodeopt2".to_vec()),
            in_memory_extension_module_shared_library: Some(b"library".to_vec()),
            in_memory_resources: Some(resources),
            in_memory_package_distribution: Some(distribution),
            in_memory_shared_library: Some(b"library".to_vec()),
            shared_library_dependency_names: Some(vec![
                "libfoo".to_string(),
                "depends".to_string(),
            ]),
        };

        let mut data = Vec::new();
        write_embedded_resources_v1(&[resource], &mut data).unwrap();
        let mut state = PythonImporterState::default();
        state.load_resources(&data).unwrap();

        assert_eq!(state.resources.len(), 1);

        let entry = state.resources.get("module").unwrap();

        assert!(entry.is_package);
        assert!(entry.is_namespace_package);
        assert_eq!(entry.in_memory_source.unwrap(), b"source");
        assert_eq!(entry.in_memory_bytecode.unwrap(), b"bytecode");
        assert_eq!(entry.in_memory_bytecode_opt1.unwrap(), b"bytecodeopt1");
        assert_eq!(entry.in_memory_bytecode_opt2.unwrap(), b"bytecodeopt2");
        assert_eq!(
            entry.in_memory_shared_library_extension_module.unwrap(),
            b"library"
        );

        let resources = entry.in_memory_resources.as_ref().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources.get("foo").unwrap(), b"foovalue");
        assert_eq!(resources.get("resource2").unwrap(), b"value2");

        let resources = entry.in_memory_package_distribution.as_ref().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources.get("dist").unwrap(), b"distvalue");
        assert_eq!(resources.get("dist2").unwrap(), b"dist2value");

        assert_eq!(entry.in_memory_shared_library.unwrap(), b"library");
        assert_eq!(
            entry.shared_library_dependency_names.as_ref().unwrap(),
            &vec!["libfoo", "depends"]
        );
    }
}