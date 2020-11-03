use crate::Result;
use crate::{Document, Object, ObjectId, StringFormat};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::io::Write;

impl Document {
    /// Change producer of document information dictionary.
    pub fn change_producer(&mut self, producer: &str) {
        if let Ok(info) = self.trailer.get_mut(b"Info") {
            if let Some(dict) = match *info {
                Object::Dictionary(ref mut dict) => Some(dict),
                Object::Reference(ref id) => self.objects.get_mut(id).and_then(|o| o.as_dict_mut().ok()),
                _ => None,
            } {
                dict.set("Producer", Object::string_literal(producer));
            }
        }
    }

    /// Compress PDF stream objects.
    pub fn compress(&mut self) {
        for object in self.objects.values_mut() {
            if let Object::Stream(ref mut stream) = *object {
                if stream.allows_compression {
                    // Ignore any error and continue to compress other streams.
                    let _ = stream.compress();
                }
            }
        }
    }

    /// Decompress PDF stream objects.
    pub fn decompress(&mut self) {
        for object in self.objects.values_mut() {
            if let Object::Stream(ref mut stream) = *object {
                stream.decompress()
            }
        }
    }

    /// Delete pages.
    pub fn delete_pages(&mut self, page_numbers: &[u32]) {
        let pages = self.get_pages();
        for page_number in page_numbers {
            if let Some(page) = pages.get(&page_number).and_then(|page_id| self.delete_object(*page_id)) {
                let mut page_tree_ref = page
                    .as_dict()
                    .and_then(|dict| dict.get(b"Parent"))
                    .and_then(Object::as_reference);
                while let Ok(page_tree_id) = page_tree_ref {
                    if let Some(page_tree) = self.objects.get_mut(&page_tree_id).and_then(|pt| pt.as_dict_mut().ok()) {
                        if let Ok(count) = page_tree.get(b"Count").and_then(Object::as_i64) {
                            page_tree.set("Count", count - 1);
                        }
                        page_tree_ref = page_tree.get(b"Parent").and_then(Object::as_reference);
                    } else {
                        break;
                    }
                }
            }
        }
    }

    /// Prune all unused objects.
    pub fn prune_objects(&mut self) -> Vec<ObjectId> {
        let mut ids = vec![];
        let refs = self.traverse_objects(|_| {});
        for id in self.objects.keys() {
            if !refs.contains(id) {
                ids.push(*id);
            }
        }

        for id in &ids {
            self.objects.remove(id);
        }

        ids
    }

    /// Delete object by object ID.
    pub fn delete_object(&mut self, id: ObjectId) -> Option<Object> {
        let action = |object: &mut Object| match *object {
            Object::Array(ref mut array) => {
                if let Some(index) = array.iter().position(|item: &Object| match *item {
                    Object::Reference(ref_id) => ref_id == id,
                    _ => false,
                }) {
                    array.remove(index);
                }
            }
            Object::Dictionary(ref mut dict) => {
                let keys: Vec<Vec<u8>> = dict
                    .iter()
                    .filter(|&(_, item): &(&Vec<u8>, &Object)| match *item {
                        Object::Reference(ref_id) => ref_id == id,
                        _ => false,
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in keys {
                    dict.remove(&key);
                }
            }
            _ => {}
        };
        self.traverse_objects(action);
        self.objects.remove(&id)
    }

    /// Delete zero length stream objects.
    pub fn delete_zero_length_streams(&mut self) -> Vec<ObjectId> {
        let mut ids = vec![];
        for id in self.objects.keys() {
            if self
                .objects
                .get(id)
                .and_then(|o| Object::as_stream(o).ok())
                .map(|stream| stream.content.is_empty())
                .unwrap_or(false)
            {
                ids.push(*id);
            }
        }

        for id in &ids {
            self.delete_object(*id);
        }

        ids
    }

    /// Renumber objects, normally called after delete_unused_objects.
    pub fn renumber_objects(&mut self) {
        self.renumber_objects_with(1)
    }

    /// Renumber objects with a custom starting id, this is very useful in case of multiple
    /// document objects insertion in a single main document
    pub fn renumber_objects_with(&mut self, starting_id: u32) {
        let mut replace = BTreeMap::new();
        let mut new_id = starting_id;
        let mut ids = self.objects.keys().cloned().collect::<Vec<ObjectId>>();
        ids.sort();

        for id in ids {
            if id.0 != new_id {
                replace.insert(id, (new_id, id.1));
            }

            new_id += 1;
        }

        let mut objects = BTreeMap::new();

        // remove and collect all removed objects
        for (old, new) in &replace {
            if let Some(object) = self.objects.remove(old) {
                objects.insert(new.clone(), object);
            }
        }

        // insert new replaced keys objects
        for (new, object) in objects {
            self.objects.insert(new, object);
        }

        let action = |object: &mut Object| {
            if let Object::Reference(ref mut id) = *object {
                if replace.contains_key(&id) {
                    *id = replace[id];
                }
            }
        };

        self.traverse_objects(action);

        self.max_id = new_id - 1;
    }

    pub fn change_content_stream(&mut self, stream_id: ObjectId, content: Vec<u8>) {
        if let Some(content_stream) = self.objects.get_mut(&stream_id) {
            if let Object::Stream(ref mut stream) = *content_stream {
                stream.set_plain_content(content);
                // Ignore any compression error.
                let _ = stream.compress();
            }
        }
    }

    pub fn change_page_content(&mut self, page_id: ObjectId, content: Vec<u8>) -> Result<()> {
        let contents = self.get_dictionary(page_id).and_then(|page| page.get(b"Contents"))?;
        match *contents {
            Object::Reference(id) => self.change_content_stream(id, content),
            Object::Array(ref arr) => {
                if arr.len() == 1 {
                    if let Ok(id) = arr[0].as_reference() {
                        self.change_content_stream(id, content)
                    }
                } else {
                    let new_stream = self.add_object(super::Stream::new(dictionary! {}, content));
                    if let Ok(page) = self.get_object_mut(page_id) {
                        if let Object::Dictionary(ref mut dict) = *page {
                            dict.set("Contents", new_stream);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn extract_stream_to_path(
        &self, stream_id: ObjectId, decompress: bool, out_path: &std::path::PathBuf,
    ) -> Result<()> {
        let mut file = File::create(out_path)?;
        if let Ok(stream_obj) = self.get_object(stream_id) {
            if let Object::Stream(ref stream) = *stream_obj {
                if decompress {
                    if let Ok(data) = stream.decompressed_content() {
                        file.write_all(&data)?;
                    } else {
                        file.write_all(&stream.content)?;
                    }
                } else {
                    file.write_all(&stream.content)?;
                }
            }
        }
        Ok(())
    }

    pub fn extract_stream(&self, stream_id: ObjectId, decompress: bool) -> Result<()> {
        let out_path = std::path::PathBuf::from(format!("{:?}.bin", stream_id));
        self.extract_stream_to_path(stream_id, decompress, &out_path)
    }

    pub fn list_attachments(&self) -> Result<()> {
        let catalog_obj = self.catalog().unwrap();
        if let Ok(names) = catalog_obj.get(b"Names") {
            if let Some(names_dict) = match *names {
                Object::Dictionary(ref dict) => Some(dict),
                Object::Reference(ref id) => self.objects.get(id).and_then(|o| o.as_dict().ok()),
                _ => None,
            } {
                if let Ok(ef) = names_dict.get(b"EmbeddedFiles") {
                    if let Some(ef_dict) = match *ef {
                        Object::Dictionary(ref dict) => Some(dict),
                        Object::Reference(ref id) => self.objects.get(id).and_then(|o| o.as_dict().ok()),
                        _ => None,
                    } {
                        if let Ok(names_tree) = ef_dict.get(b"Names") {
                            if let Some(name_tree) = match *names_tree {
                                Object::Array(ref arr) => Some(arr),
                                Object::Reference(ref id) => self.objects.get(id).and_then(|o| o.as_array().ok()),
                                _ => None,
                            } {
                                for item in name_tree.iter() {
                                    match *item {
                                        Object::Dictionary(ref _dict) => {}
                                        Object::String(ref string, _) => {
                                            println!("{}", String::from_utf8_lossy(string));
                                        }
                                        Object::Reference(ref _id) => {}
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn add_attachment(&mut self, path_to_file: &str) -> Result<()> {
        println!("Adding '{}'", path_to_file);

        let orig_catalog = self.catalog().unwrap();
        let mut catalog = orig_catalog.clone();
        let mut names = match catalog.get(b"Names") {
            Ok(n) => n.clone(),
            Err(_e) => {
                let names_tree_id = self.new_object_id();
                let names_id = self.add_object(dictionary! {
                    "EmbeddedFiles" => dictionary! {
                            "Names" => names_tree_id,
                    },
                });
                self.objects.insert(names_tree_id, Object::Array(vec![]));
                self.get_object(names_id)?.clone()
            }
        };

        let mut ef_dict = match names.as_dict()?.get(b"EmbeddedFiles") {
            Ok(ef) => ef.clone(),
            Err(_e) => {
                let names_tree_id = self.new_object_id();
                let ef_id = self.add_object(dictionary! {
                    "Names" => names_tree_id,
                });
                self.objects.insert(names_tree_id, Object::Array(vec![]));
                names.as_dict_mut()?.set("EmbeddedFiles", Object::Reference(ef_id));
                self.get_object(ef_id)?.clone()
            }
        };

        if let Ok(names_tree) = ef_dict.as_dict()?.get(b"Names") {
            if let Some(name_arr) = match *names_tree {
                Object::Array(ref arr) => Some(arr),
                Object::Reference(ref id) => self.objects.get(id).and_then(|o| o.as_array().ok()),
                _ => None,
            } {
                let mut new_names = name_arr.clone();

                let file_path = std::path::PathBuf::from(path_to_file);
                let file_name = file_path.file_name().unwrap().to_str().unwrap();
                new_names.push(Object::String(file_name.into(), StringFormat::Literal));

                let mut buffer = Vec::new();
                let mut f = File::open(file_path.clone())?;
                f.read_to_end(&mut buffer)?;
                let mut fs_obj = super::Stream::new(
                    dictionary! {
                        "DL" => Object::Integer(buffer.len() as i64),
                        "Params" => dictionary!{
                            "Size" => Object::Integer(buffer.len() as i64),
                        },
                    },
                    buffer,
                );
                fs_obj.compress().unwrap();
                let file_stream_id = self.add_object(fs_obj);
                let ef_id = self.add_object(dictionary! {
                    "F" => file_stream_id,
                });

                let filespec = self.add_object(dictionary! {
                    "Type" => "Filespec",
                    "F" => Object::String(file_name.into(), StringFormat::Literal),
                    "EF" => ef_id,
                });
                new_names.push(Object::Reference(filespec));

                ef_dict.as_dict_mut()?.set("Names", Object::Array(new_names));
                names.as_dict_mut()?.set("EmbeddedFiles", ef_dict);
            }
        };

        catalog.set("Names", names);
        self.trailer.set("Root", catalog);

        Ok(())
    }
}
