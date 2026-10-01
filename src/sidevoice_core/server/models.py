"""The model catalogue, as this node serves it to its clients (`sidevoice_core.models`), and the check a
provider's model passes before a client makes it its stage.

Behind a paired device's token like every node route, and relayed like the rest of `/api/models`. The catalogue
is served exactly as shipped, so what a client reads here is byte for byte what the web build and the desktop
app were generated from.
"""
from fastapi import HTTPException, Request, Response
from pydantic import ValidationError

from ..models.catalog import catalog_text
from .presentation import require_same_origin

# A device checks the models it runs itself (it has the clips too); only a provider is checked from here.
CHECK_ON_DEVICE = {'key': 'check_on_device', 'message': 'A device checks the models it runs itself.'}


def mount_models(app):
    @app.get('/api/models/catalog')
    async def model_catalog(request: Request):
        require_same_origin(request)
        return Response(catalog_text(), media_type='application/json')

    @app.post('/api/models/check')
    async def model_check(payload: dict, request: Request):
        """`{stage: 'stt'|'tts', place, model, options, language?}` → `{ok, step, reason?, passes, latency_ms…}`.
        A check that fails is an answer (200, `ok: false`); a request that cannot be checked here is refused:
        the host, which runs no models yet, a device, which checks itself, or a stage that is not valid."""
        require_same_origin(request)
        from ..pipeline import model_check as checks
        from ..pipeline.settings import HOST_UNAVAILABLE, STAGES
        task = payload.get('stage')
        if task not in STAGES:
            raise HTTPException(422, {'key': 'check_invalid', 'message': 'stage must be stt or tts.'})
        if payload.get('place') == 'host':
            raise HTTPException(409, dict(HOST_UNAVAILABLE))
        if payload.get('place') == 'device':
            raise HTTPException(400, dict(CHECK_ON_DEVICE))
        try:
            stage = STAGES[task](place=payload.get('place') or '', model=payload.get('model') or '',
                                 options=payload.get('options') or {})
        except ValidationError as error:
            detail = '; '.join(item.get('msg', '') for item in error.errors()[:3])
            raise HTTPException(422, {'key': 'check_invalid', 'message': detail}) from error
        language = payload.get('language')
        result = await checks.check(stage, language=language if isinstance(language, str) else None)
        return {'stage': task, 'place': stage.place, 'model': stage.model, **result}
