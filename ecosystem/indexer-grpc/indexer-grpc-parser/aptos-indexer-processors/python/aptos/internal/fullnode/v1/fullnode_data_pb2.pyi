from aptos.transaction.v1 import transaction_pb2 as _transaction_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Iterable as _Iterable, Mapping as _Mapping, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TransactionsOutput(_message.Message):
    __slots__ = ["transactions"]
    TRANSACTIONS_FIELD_NUMBER: _ClassVar[int]
    transactions: _containers.RepeatedCompositeFieldContainer[_transaction_pb2.Transaction]
    def __init__(self, transactions: _Optional[_Iterable[_Union[_transaction_pb2.Transaction, _Mapping]]] = ...) -> None: ...

class StreamStatus(_message.Message):
    __slots__ = ["type", "start_version", "end_version"]
    class StatusType(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
        __slots__ = []
        STATUS_TYPE_UNSPECIFIED: _ClassVar[StreamStatus.StatusType]
        STATUS_TYPE_INIT: _ClassVar[StreamStatus.StatusType]
        STATUS_TYPE_BATCH_END: _ClassVar[StreamStatus.StatusType]
    STATUS_TYPE_UNSPECIFIED: StreamStatus.StatusType
    STATUS_TYPE_INIT: StreamStatus.StatusType
    STATUS_TYPE_BATCH_END: StreamStatus.StatusType
    TYPE_FIELD_NUMBER: _ClassVar[int]
    START_VERSION_FIELD_NUMBER: _ClassVar[int]
    END_VERSION_FIELD_NUMBER: _ClassVar[int]
    type: StreamStatus.StatusType
    start_version: int
    end_version: int
    def __init__(self, type: _Optional[_Union[StreamStatus.StatusType, str]] = ..., start_version: _Optional[int] = ..., end_version: _Optional[int] = ...) -> None: ...

class GetTransactionsFromNodeRequest(_message.Message):
    __slots__ = ["starting_version", "transactions_count"]
    STARTING_VERSION_FIELD_NUMBER: _ClassVar[int]
    TRANSACTIONS_COUNT_FIELD_NUMBER: _ClassVar[int]
    starting_version: int
    transactions_count: int
    def __init__(self, starting_version: _Optional[int] = ..., transactions_count: _Optional[int] = ...) -> None: ...

class TransactionsFromNodeResponse(_message.Message):
    __slots__ = ["status", "data", "chain_id"]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    CHAIN_ID_FIELD_NUMBER: _ClassVar[int]
    status: StreamStatus
    data: TransactionsOutput
    chain_id: int
    def __init__(self, status: _Optional[_Union[StreamStatus, _Mapping]] = ..., data: _Optional[_Union[TransactionsOutput, _Mapping]] = ..., chain_id: _Optional[int] = ...) -> None: ...
