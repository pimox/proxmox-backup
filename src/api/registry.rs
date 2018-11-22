use crate::api::schema::*;

use failure::*;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Registry {
    formats: HashMap<&'static str, Arc<ApiStringFormat>>,
    options: HashMap<&'static str, Arc<Schema>>,
}

impl Registry {

    pub fn new() -> Self {

        let mut me = Self {
            formats: HashMap::new(),
            options: HashMap::new(),
        };

        me.initialize_formats();

        me.initialize_options();

        me
    }

    pub fn register_format(&mut self, name: &'static str, format: ApiStringFormat) {

        if let Some(format) = self.formats.get(name) {
            panic!("standard format '{}' already registered.", name); // fixme: really panic?
        }

        self.formats.insert(name, Arc::new(format));
    }

    pub fn lookup_format(&self, name: &str) -> Option<Arc<ApiStringFormat>> {

        if let Some(format) = self.formats.get(name) {
            return Some(format.clone());
        }
        None
    }

    pub fn register_option(&mut self, name: &'static str, schema: Schema) {

        if let Some(schema) = self.options.get(name) {
            panic!("standard option '{}' already registered.", name); // fixme: really panic?
        }

        self.options.insert(name, Arc::new(schema));
    }

    pub fn lookup_option(&self, name: &str) -> Option<Arc<Schema>> {

        if let Some(schema) = self.options.get(name) {
            return Some(schema.clone());
        }
        None
    }

    fn initialize_formats(&mut self) {

        self.register_format("pve-node", ApiStringFormat::VerifyFn(verify_pve_node));

    }

    fn initialize_options(&mut self) {

        self.register_option(
            "pve-vmid",
            Integer!{
                description => "The (unique) ID of the VM.",
                minimum => Some(1),
                optional => false
            });

        self.register_option(
            "pve-node",
            ApiString!{
                description => "The cluster node name.",
                format => self.lookup_format("pve-node")
            });
     }

}

fn verify_pve_node(value: &str) -> Result<(), Error> {

    Ok(())
}
