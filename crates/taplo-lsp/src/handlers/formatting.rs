use lsp_async_stub::{rpc::Error, Context, Params};
use lsp_types::{DocumentFormattingParams, TextEdit};
use taplo::formatter;
use taplo_common::environment::Environment;

use crate::World;

#[tracing::instrument(skip_all)]
pub(crate) async fn format<E: Environment>(
    context: Context<World<E>>,
    params: Params<DocumentFormattingParams>,
) -> Result<Option<Vec<TextEdit>>, Error> {
    let p = params.required()?;

    let Some(document_uri) = crate::uri::to_url(&p.text_document.uri) else {
        return Ok(None);
    };

    let Some(snapshot) = context.document_snapshot(&document_uri).await else {
        return Ok(None);
    };
    let doc = &snapshot.document;

    let doc_path = context
        .env
        .to_file_path_normalized(&document_uri)
        .ok_or_else(|| {
            Error::invalid_request().with_data(format!(
                "invalid (non-local) uri for file: {document_uri}"
            ))
        })?;

    let tab_size = usize::try_from(p.options.tab_size)
        .map_err(|_| Error::invalid_params().with_data("tab size is not representable"))?;
    let mut format_opts = formatter::Options {
        indent_string: if p.options.insert_spaces {
            " ".repeat(tab_size)
        } else {
            "\t".into()
        },
        ..Default::default()
    };

    if let Some(v) = p.options.insert_final_newline {
        format_opts.trailing_newline = v;
    }

    format_opts.update_camel(snapshot.config.formatter.clone());

    snapshot
        .taplo_config
        .update_format_options(&doc_path, &mut format_opts);

    let scopes = snapshot.taplo_config.format_scopes(&doc_path);
    tracing::trace!(
        ?doc_path,
        ?format_opts,
        scopes = ?scopes.clone().collect::<Vec<_>>(),
        all_rules = ?snapshot.taplo_config.rule,
        matched_rules = ?snapshot.taplo_config.rules_for(&doc_path).collect::<Vec<_>>(),
    );

    let range = crate::uri::mapper_range_to_lsp(doc.mapper.all_range()).ok_or_else(|| {
        Error::internal_error().with_data("document range is not representable by LSP")
    })?;

    Ok(Some(vec![TextEdit {
        range,
        new_text: taplo::formatter::format_with_path_scopes(
            doc.dom.clone(),
            format_opts,
            &doc.parse
                .errors
                .iter()
                .map(|err| err.range)
                .collect::<Vec<_>>(),
            scopes.into_iter(),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "invalid key pattern");
            Error::internal_error().with_data("invalid Taplo configuration")
        })?,
    }]))
}
