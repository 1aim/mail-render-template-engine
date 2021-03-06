use std::collections::HashMap;
use std::path::Path;

use mail::{Resource, Context};
use mail::file_buffer::FileBuffer;

use template::TemplateEngine;
use template::{
    EmbeddedWithCId,
    BodyPart, MailParts
};

use ::error::{LoadingError, InsertionError};
use ::utils::fix_newlines;
use ::spec::TemplateSpec;
use ::traits::{RenderEngine, RenderEngineBase, AdditionalCIds};
use ::settings::LoadSpecSettings;

#[derive(Debug)]
pub struct RenderTemplateEngine<R>
    where R: RenderEngineBase
{
    fix_newlines: bool,
    render_engine: R,
    id2spec: HashMap<String, TemplateSpec>,
}


impl<R> RenderTemplateEngine<R>
    where R: RenderEngineBase
{

    pub fn new(render_engine: R) -> Self {
        RenderTemplateEngine {
            render_engine,
            id2spec: Default::default(),
            fix_newlines: !R::PRODUCES_VALID_NEWLINES,
        }
    }

    pub fn set_fix_newlines(&mut self, should_fix_newlines: bool) {
        self.fix_newlines = should_fix_newlines
    }

    pub fn does_fix_newlines(&self) -> bool {
        self.fix_newlines
    }

    /// add a `TemplateSpec`, loading all templates in it
    ///
    /// If a template with the same name is contained it
    /// will be removed (and unloaded and returned).
    ///
    /// If a template replaces a new template the old
    /// template is first unloaded and then the new
    /// template is loaded.
    ///
    /// # Error
    ///
    /// If the render templates where already loaded or can not
    /// be loaded an error is returned.
    ///
    /// If an error occurs when loading a new spec which _replaces_
    /// an old spec the old spec is already removed and unloaded.
    /// I.e. it's guaranteed that if `insert` errors there will no
    /// longer be an template associated with the given id.
    ///
    pub fn insert_spec(
        &mut self,
        id: String,
        spec: TemplateSpec
    ) -> Result<Option<TemplateSpec>, InsertionError<R::LoadingError>> {
        use std::collections::hash_map::Entry::*;
        match self.id2spec.entry(id) {
            Occupied(mut entry) => {
                let old = entry.insert(spec);
                self.render_engine.unload_templates(&old);
                let res = self.render_engine.load_templates(entry.get());
                if let Err(error) = res {
                    let (_, failed_new_value) = entry.remove_entry();
                    Err(InsertionError {
                        error, failed_new_value,
                        old_value: Some(old)
                    })
                } else {
                    Ok(Some(old))
                }
            },
            Vacant(entry) => {
                let res = self.render_engine.load_templates(&spec);
                if let Err(error) = res {
                    Err(InsertionError {
                        error, failed_new_value: spec,
                        old_value: None
                    })
                } else {
                    entry.insert(spec);
                    Ok(None)
                }
            }
        }
    }

    /// removes and unload the spec associated with the given id
    ///
    /// If no spec is associated with the given id nothing is done
    /// (and `None` is returned).
    pub fn remove_spec(&mut self, id: &str) -> Option<TemplateSpec> {
        let res =  self.id2spec.remove(id);
        if let Some(spec) = res.as_ref() {
            self.render_engine.unload_templates(spec);
        }
        res
    }

    pub fn specs(&self) -> &HashMap<String, TemplateSpec> {
        &self.id2spec
    }

    pub fn specs_mut(&mut self) -> impl Iterator<Item=(&String, &mut TemplateSpec)> {
        self.id2spec.iter_mut()
    }

    pub fn lookup_spec(&self, template_id: &str) -> Option<&TemplateSpec> {
        self.id2spec.get(template_id)
    }

    /// each folder in `templates_dir` is seen as a TemplateSpec
    ///
    /// # Error
    ///
    /// If an error can occur when creating the spec(s), or when inserting/using
    /// them. If such an error occurs all previously added Spec are not removed,
    /// i.e. if an error happens some spec and embeddings might be added others
    /// might not.
    pub fn load_templates(
        &mut self,
        templates_dir: impl AsRef<Path>,
        settings: &LoadSpecSettings
    ) -> Result<(), LoadingError<R::LoadingError>> {
        for (name, spec) in TemplateSpec::from_dirs(templates_dir.as_ref(), settings)? {
            self.insert_spec(name, spec)?;
        }
        Ok(())
    }
}

impl<C, D, R> TemplateEngine<C, D> for RenderTemplateEngine<R>
    where C: Context, R: RenderEngine<D>
{
    type TemplateId = str;
    type Error = <R as RenderEngineBase>::RenderError;

    fn use_template(
        &self,
        template_id: &str,
        data: &D,
        ctx: &C,
    ) -> Result<MailParts, Self::Error >
    {
        let spec = self.lookup_spec(template_id)
            .ok_or_else(|| R::unknown_template_id_error(template_id))?;

        //OPTIMIZE there should be a more efficient way
        // maybe use Rc<str> as keys? and Rc<Resource> for embeddings?
        let shared_embeddings = spec.embeddings().iter()
            .map(|(key, resource)| create_embedding(key, resource, ctx))
            .collect::<HashMap<_,_>>();

        let bodies = spec.sub_specs().try_mapped_ref(|sub_spec| {

            let embeddings = sub_spec.embeddings().iter()
                .map(|(key, resource)| create_embedding(key, resource, ctx))
                .collect::<HashMap<_,_>>();

            let rendered = {
                let embeddings = &[&embeddings, &shared_embeddings];
                let additional_cids = AdditionalCIds::new(embeddings);
                self.render_engine.render(sub_spec, data, additional_cids)?
            };

            let rendered =
                if self.fix_newlines {
                    fix_newlines(rendered)
                } else {
                    rendered
                };

            let buffer = FileBuffer::new(sub_spec.media_type().clone(), rendered.into());
            let resource = Resource::sourceless_from_buffer(buffer);

            Ok(BodyPart {
                resource: resource,
                embeddings: embeddings.into_iter().map(|(_,v)| v).collect()
            })
        })?;

        let attachments = spec.attachments().iter()
            .map(|resource| EmbeddedWithCId::attachment(resource.clone(), ctx))
            .collect();

        Ok(MailParts {
            alternative_bodies: bodies,
            //TODO collpas embeddings and attachments and use their disposition parma
            // instead
            shared_embeddings: shared_embeddings.into_iter().map(|(_, v)| v).collect(),
            attachments,
        })
    }
}

fn create_embedding(
    key: &str,
    resource: &Resource,
    ctx: &impl Context
) -> (String, EmbeddedWithCId)
{
    (key.to_owned(), EmbeddedWithCId::inline(resource.clone(), ctx))
}