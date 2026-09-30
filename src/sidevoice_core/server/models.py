"""The model catalogue, as this node serves it to its clients (`sidevoice_core.models`).

Behind a paired device's token like every node route. It serves the file exactly as shipped, so what a
client reads here is byte for byte what the web build and the desktop app were generated from.
"""
from fastapi import Request, Response

from ..models.catalog import catalog_text
from .presentation import require_same_origin


def mount_models(app):
    @app.get('/api/models/catalog')
    async def model_catalog(request: Request):
        require_same_origin(request)
        return Response(catalog_text(), media_type='application/json')
