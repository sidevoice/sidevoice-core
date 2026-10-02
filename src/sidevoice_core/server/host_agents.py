"""The authenticated host view of the coding agents known to this machine.

The connector owns detection and registration. The core only asks its local connector link for a
result; it never accepts a command, executable or path from a page.
"""
import asyncio
import re

from fastapi import HTTPException, Request
from fastapi.responses import JSONResponse
from loguru import logger

from .presentation import require_same_origin

REQUEST_TIMEOUT_SECONDS = 20.0
AGENT_ID = re.compile(r'^[a-z0-9][a-z0-9._-]{0,99}$')
ERROR_KEY = re.compile(r'^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$')
UNSAFE_PARAM = re.compile(r'(?:command|executable|stderr|stdout|raw|output|message|detail|error)', re.I)


def safe_params(value, *, depth=0):
    """Keep only small, structured i18n parameters; never forward a connector's raw error text."""
    if depth > 4:
        return None
    if value is None or isinstance(value, (bool, int)):
        return value
    if isinstance(value, str):
        return value[:500]
    if isinstance(value, list):
        return [item for child in value[:24] if (item := safe_params(child, depth=depth + 1)) is not None]
    if isinstance(value, dict):
        return {
            key: cleaned
            for key, child in list(value.items())[:24]
            if isinstance(key, str) and len(key) <= 64 and not UNSAFE_PARAM.search(key)
            and (cleaned := safe_params(child, depth=depth + 1)) is not None
        }
    return None


def keyed_error(key, status, params=None):
    body = {'key': key if isinstance(key, str) and ERROR_KEY.fullmatch(key) else 'connector-error'}
    cleaned = safe_params(params)
    if isinstance(cleaned, dict) and cleaned:
        body['params'] = cleaned
    return JSONResponse(body, status_code=status)


def connector_error(answer):
    error = answer.get('error') if isinstance(answer, dict) else None
    if not isinstance(error, dict):
        return None
    key = error.get('key')
    error_body = {'key': key if isinstance(key, str) and ERROR_KEY.fullmatch(key) else 'connector-error'}
    params = safe_params(error.get('params'))
    if isinstance(params, dict) and params:
        error_body['params'] = params
    # The connector's message is a curated, bundle-derived summary. Raw process output is never
    # accepted here; only the known message field, bounded for an unexpectedly old connector.
    message = error.get('message')
    if isinstance(message, str):
        error_body['message'] = message[:500]
    return JSONResponse({'error': error_body}, status_code=409)


def peer_for(control):
    peers = getattr(control, 'peers', None) or {}
    return next(reversed(peers.values()), None) if peers else None


def origin_refusal(request):
    try:
        require_same_origin(request)
    except HTTPException as error:
        return keyed_error('origin-not-allowed', error.status_code)
    return None


async def request_connector(control, event, data):
    peer = peer_for(control)
    if peer is None:
        return None, keyed_error('no-connector', 503)
    try:
        # The Socket.IO peer also uses this budget for its acknowledgement. The outer deadline
        # makes the contract hold for every peer implementation, including test and future ones.
        answer = await asyncio.wait_for(
            peer.request(event, data, timeout=REQUEST_TIMEOUT_SECONDS),
            timeout=REQUEST_TIMEOUT_SECONDS,
        )
    except (asyncio.TimeoutError, TimeoutError):
        return None, keyed_error('connector-timeout', 504)
    except Exception as error:
        logger.warning('Host agent request {} failed: {}', event, type(error).__name__)
        return None, keyed_error('connector-unavailable', 502)
    if not isinstance(answer, dict):
        return None, keyed_error('invalid-connector-response', 502)
    failure = connector_error(answer)
    if failure is not None:
        return None, failure
    return answer, None


def state_response(answer):
    agents = answer.get('agents')
    custom = answer.get('custom')
    if not isinstance(agents, list) or not all(isinstance(agent, dict) for agent in agents) \
            or not isinstance(custom, dict):
        return keyed_error('invalid-connector-response', 502)
    # These fields are connector-owned observations and manual snippets. No execution occurs here.
    return {'agents': agents, 'scanned_at': answer.get('scanned_at'), 'custom': custom}


def mount_host_agents(app, control):
    """Mount the host-agent routes. Authentication is provided by DeviceAuth in app assembly."""

    @app.get('/api/host/agents')
    async def list_agents(request: Request, rescan: str | None = None, watch: str | None = None):
        refusal = origin_refusal(request)
        if refusal is not None:
            return refusal
        rescan_value = rescan.casefold() if rescan is not None else None
        if rescan_value in {None, '0', 'false'}:
            force_scan = False
        elif rescan_value in {'1', 'true'}:
            force_scan = True
        else:
            return keyed_error('invalid-rescan', 400)
        if watch is not None and not AGENT_ID.fullmatch(watch):
            return keyed_error('invalid-agent-id', 400)
        answer, failure = await request_connector(
            control, 'agents.list', {'rescan': force_scan, **({'watch': watch} if watch is not None else {})}
        )
        if failure is not None:
            return failure
        return state_response(answer)

    async def act(agent_id, action, request):
        refusal = origin_refusal(request)
        if refusal is not None:
            return refusal
        if not AGENT_ID.fullmatch(agent_id):
            return keyed_error('invalid-agent-id', 400)
        answer, failure = await request_connector(control, f'agents.{action}', {'id': agent_id})
        if failure is not None:
            return failure
        return state_response(answer)

    @app.post('/api/host/agents/{agent_id}/connect')
    async def connect_agent(agent_id: str, request: Request):
        return await act(agent_id, 'connect', request)

    @app.post('/api/host/agents/{agent_id}/disconnect')
    async def disconnect_agent(agent_id: str, request: Request):
        return await act(agent_id, 'disconnect', request)

    @app.post('/api/host/agents/{agent_id}/dismiss')
    async def dismiss_agent(agent_id: str, request: Request):
        return await act(agent_id, 'dismiss', request)
